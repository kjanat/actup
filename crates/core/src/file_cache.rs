//! Disk cache for version sets, shared by the CLI, the LSP and `actup-mcp`.
//!
//! Stores cached version data in:
//! - Linux: `~/.cache/actup/`
//! - macOS: `~/Library/Caches/actup/`
//! - Windows: `%LOCALAPPDATA%\actup\cache\`

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use tracing::debug;

use crate::{
    Config,
    atomic_write::write_atomic,
    cache::{self, Cache},
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
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::version;

    fn make_test_cache(ttl_secs: u64) -> (FileCache, TempDir) {
        let temp = TempDir::new().unwrap();
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_secs(ttl_secs));
        (cache, temp)
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let (cache, _temp) = make_test_cache(3600);

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry.clone()).await.unwrap();

        let retrieved = cache.get("test_key").await.unwrap();
        assert_eq!(retrieved.versions().semver_tags(), []);
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let (cache, _temp) = make_test_cache(3600);

        let result = cache.get("nonexistent").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn test_expired_entry_returns_not_found() {
        let (cache, _temp) = make_test_cache(0); // 0 TTL = immediately expired

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        // Should be expired immediately
        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn expired_entry_stays_readable_through_lookup() -> Result<(), Box<dyn std::error::Error>>
    {
        let (cache, _temp) = make_test_cache(0);
        cache
            .set("key", cache::Entry::new(version::Set::default()))
            .await?;

        assert!(cache.get("key").await.is_err());
        assert!(matches!(
            cache.lookup("key").await?,
            cache::Lookup::Expired(_)
        ));

        let (fresh, _temp) = make_test_cache(3600);
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

    #[tokio::test]
    async fn test_remove() {
        let (cache, _temp) = make_test_cache(3600);

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        // Verify it exists
        assert!(cache.get("test_key").await.is_ok());

        // Remove it
        cache.remove("test_key").await.unwrap();

        // Verify it's gone
        assert!(matches!(
            cache.get("test_key").await,
            Err(cache::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_clear() {
        let (cache, _temp) = make_test_cache(3600);

        // Add multiple entries
        for i in 0..5 {
            let entry = cache::Entry::new(version::Set::default());
            cache.set(&format!("key_{i}"), entry).await.unwrap();
        }

        // Clear all
        cache.clear().await.unwrap();

        // Verify all gone
        for i in 0..5 {
            let result = cache.get(&format!("key_{i}")).await;
            assert!(matches!(result, Err(cache::Error::NotFound)));
        }
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
        let (cache, temp) = make_test_cache(3600);
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
