//! File-based persistent cache for version information.
//!
//! Shares the same on-disk directory as the CLI (`~/.cache/actup/`) so
//! cache entries are reusable across both tools.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use actup_core::cache::{self, Cache};

/// File-based cache implementation that persists version data to disk.
///
/// Uses OS-appropriate cache directories:
/// - Linux: `~/.cache/actup/`
/// - macOS: `~/Library/Caches/actup/`
/// - Windows: `%LOCALAPPDATA%\actup\cache\`
pub struct FileCache {
    cache_dir: PathBuf,
    ttl: Duration,
    enabled: bool,
}

impl FileCache {
    /// Create a new file cache with the default OS cache directory.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            cache_dir: default_cache_dir(),
            ttl,
            enabled: true,
        }
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

    /// Convert a cache key to a filesystem path.
    fn key_to_path(&self, key: &str) -> PathBuf {
        let safe_key = sanitize_filename::sanitize(key);
        self.cache_dir.join(format!("{safe_key}.json"))
    }

    /// Ensure the cache directory exists.
    async fn ensure_dir(&self) -> cache::Result<()> {
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .map_err(|e| cache::Error::Io(format!("failed to create cache dir: {e}")))?;
        Ok(())
    }
}

/// Returns the default OS cache directory for actup.
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir().map_or_else(
        || PathBuf::from(".actup-cache"),
        |p| {
            let base = p.join("actup");
            #[cfg(windows)]
            let base = base.join("cache");
            base
        },
    )
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

            let entry: cache::Entry =
                serde_json::from_str(&content).map_err(|e| cache::Error::Serde(e.to_string()))?;

            if entry.is_expired(self.ttl) {
                tracing::debug!(key = %key, "cache entry expired");
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    tracing::debug!(key = %key, error = %e, "failed to remove expired entry");
                }
                return Err(cache::Error::NotFound);
            }

            tracing::debug!(key = %key, age_secs = ?entry.age().map(|d| d.as_secs()), "cache hit");
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
                return Ok(());
            }

            self.ensure_dir().await?;

            let path = self.key_to_path(&key);
            let content =
                serde_json::to_string(&entry).map_err(|e| cache::Error::Serde(e.to_string()))?;

            tokio::fs::write(&path, content)
                .await
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
                if path.extension().is_some_and(|ext| ext == "json") {
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
    use super::*;
    use actup_core::version;
    use tempfile::TempDir;

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
        assert!(retrieved.versions().semver_tags().is_empty());
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
    fn key_to_path_sanitizes_special_chars() {
        let (cache, _temp) = make_test_cache(3600);
        let path = cache.key_to_path("github::github.com/actions/checkout");
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(!filename.contains("::"));
        assert!(!filename.contains('/'));
        assert!(
            std::path::Path::new(filename)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        );
    }
}
