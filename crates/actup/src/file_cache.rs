//! File-based persistent cache for version information
//!
//! Stores cached version data in:
//! - Linux: `~/.cache/actup/`
//! - macOS: `~/Library/Caches/actup/`
//! - Windows: `%LOCALAPPDATA%\actup\cache\`

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use actup_core::{
    Config,
    cache::{self, Cache},
    write_atomic,
};
use tracing::debug;

/// File-based cache implementation
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

    /// Create a file cache with custom directory
    #[must_use]
    pub const fn with_path(path: PathBuf, ttl: Duration) -> Self {
        Self {
            cache_dir: path,
            ttl,
            enabled: true,
        }
    }

    /// Create a disabled cache (no-op)
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            cache_dir: PathBuf::new(),
            ttl: Duration::ZERO,
            enabled: false,
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

            let entry: cache::Entry = match serde_json::from_str(&content) {
                Ok(entry) => entry,
                Err(e) => {
                    debug!(key = %key, error = %e, "unreadable cache entry");
                    return Err(cache::Error::NotFound);
                }
            };

            // Check TTL
            if entry.is_expired(self.ttl) {
                debug!(key = %key, "cache entry expired");
                // Remove expired entry (best-effort, log failures)
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    debug!(key = %key, path = %path.display(), error = %e, "failed to remove expired cache entry");
                }
                return Err(cache::Error::NotFound);
            }

            debug!(key = %key, age_secs = ?entry.age().map(|d| d.as_secs()), "cache hit");
            Ok(entry)
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
                return Ok(()); // Silently ignore writes when disabled
            }

            self.ensure_dir().await?;

            let path = self.key_to_path(&key);
            let content =
                serde_json::to_string(&entry).map_err(|e| cache::Error::Serde(e.to_string()))?;

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
        self.enabled
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use actup_core::version;
    use tempfile::TempDir;

    use super::*;

    fn make_test_cache(ttl_secs: u64) -> (FileCache, TempDir) {
        let temp = TempDir::new().unwrap();
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_secs(ttl_secs));
        (cache, temp)
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let (cache, _temp) = make_test_cache(3600);

        let entry = actup_core::cache::Entry::new(version::Set::default());
        cache.set("test_key", entry.clone()).await.unwrap();

        let retrieved = cache.get("test_key").await.unwrap();
        assert_eq!(retrieved.versions().semver_tags(), []);
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let (cache, _temp) = make_test_cache(3600);

        let result = cache.get("nonexistent").await;
        assert!(matches!(result, Err(actup_core::cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn test_expired_entry_returns_not_found() {
        let (cache, _temp) = make_test_cache(0); // 0 TTL = immediately expired

        let entry = actup_core::cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        // Should be expired immediately
        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(actup_core::cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn test_remove() {
        let (cache, _temp) = make_test_cache(3600);

        let entry = actup_core::cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        // Verify it exists
        assert!(cache.get("test_key").await.is_ok());

        // Remove it
        cache.remove("test_key").await.unwrap();

        // Verify it's gone
        assert!(matches!(
            cache.get("test_key").await,
            Err(actup_core::cache::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_clear() {
        let (cache, _temp) = make_test_cache(3600);

        // Add multiple entries
        for i in 0..5 {
            let entry = actup_core::cache::Entry::new(version::Set::default());
            cache.set(&format!("key_{i}"), entry).await.unwrap();
        }

        // Clear all
        cache.clear().await.unwrap();

        // Verify all gone
        for i in 0..5 {
            let result = cache.get(&format!("key_{i}")).await;
            assert!(matches!(result, Err(actup_core::cache::Error::NotFound)));
        }
    }

    #[tokio::test]
    async fn test_disabled_cache() {
        let cache = FileCache::disabled();

        assert!(!cache.is_enabled());

        // Set should succeed silently
        let entry = actup_core::cache::Entry::new(version::Set::default());
        assert!(cache.set("test_key", entry).await.is_ok());

        // Get should return Disabled
        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(actup_core::cache::Error::Disabled)));
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
    async fn clear_keeps_files_it_did_not_write() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        for name in ["package.json", "tsconfig.json", "notes.txt"] {
            std::fs::write(temp.path().join(name), "{}")?;
        }
        cache
            .set(
                "key",
                actup_core::cache::Entry::new(version::Set::default()),
            )
            .await?;

        cache.clear().await?;

        let mut names: Vec<_> = std::fs::read_dir(temp.path())?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        assert_eq!(names, ["notes.txt", "package.json", "tsconfig.json"]);
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
        let (cache, log) = logged(|| FileCache::in_dir(&config, None))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, format!(" WARN {}\n", cache::NO_DIR_WARNING));

        let temp = TempDir::new()?;
        let (cache, log) = logged(|| FileCache::in_dir(&config, Some(temp.path().to_path_buf())))?;
        assert!(cache.is_enabled());
        assert_eq!(cache.cache_dir(), temp.path());
        assert_eq!(log, "");

        let mut off = Config::default();
        off.cache.enabled = false;
        let (cache, log) = logged(|| FileCache::in_dir(&off, None))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, "");
        Ok(())
    }
}
