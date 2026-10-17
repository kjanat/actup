//! File-based persistent cache for version information
//!
//! Stores cached version data in:
//! - Linux: `~/.cache/actup/`
//! - macOS: `~/Library/Caches/actup/`
//! - Windows: `%LOCALAPPDATA%\actup\cache\`

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use actup_core::cache::{self, Cache};
use tracing::debug;

/// File-based cache implementation
pub struct FileCache {
    cache_dir: PathBuf,
    ttl: Duration,
    enabled: bool,
}

impl FileCache {
    /// Create a new file cache with default location
    ///
    /// Uses OS-appropriate cache directory:
    /// - Linux: `~/.cache/actup/`
    /// - macOS: `~/Library/Caches/actup/`
    /// - Windows: `%LOCALAPPDATA%\actup\cache\`
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        let cache_dir = default_cache_dir();
        Self {
            cache_dir,
            ttl,
            enabled: true,
        }
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
    #[allow(dead_code)] // Part of public API for future use
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

    /// Convert a cache key to a file path
    fn key_to_path(&self, key: &str) -> PathBuf {
        // Sanitize key for filesystem using sanitize-filename crate
        // This handles all OS-specific invalid characters (including Windows: : * ? " < > |)
        let safe_key = sanitize_filename::sanitize(key);
        self.cache_dir.join(format!("{safe_key}.json"))
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

/// Get the default cache directory for the current OS
///
/// - Linux: `~/.cache/actup/`
/// - macOS: `~/Library/Caches/actup/`
/// - Windows: `%LOCALAPPDATA%\actup\cache\`
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir().map_or_else(
        || {
            // Fallback to current directory if no cache dir available
            PathBuf::from(".actup-cache")
        },
        |p| {
            let base = p.join("actup");
            // Windows uses an extra "cache" subdirectory per convention
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

            tokio::fs::write(&path, content)
                .await
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
                if path.extension().is_some_and(|ext| ext == "json") {
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

    #[test]
    fn test_key_to_path_sanitization() {
        let (cache, _temp) = make_test_cache(3600);

        // Keys with :: and / should be sanitized
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
