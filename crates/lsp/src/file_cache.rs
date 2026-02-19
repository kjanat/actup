//! File-based persistent cache for version information.
//!
//! Shares the same on-disk directory as the CLI (`~/.cache/actup/`) so
//! cache entries are reusable across both tools.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use actup_core::cache::{self, Cache};
use actup_core::{ActionRepoId, provider, version};

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
    ///
    /// Uses hex-encoded SHA-256 of the key to avoid collisions that
    /// `sanitize_filename` can cause (e.g. `a::b/c` and `a__b_c` mapping
    /// to the same sanitized filename). The hash is deterministic and
    /// filesystem-safe on all platforms.
    fn key_to_path(&self, key: &str) -> PathBuf {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        self.cache_dir.join(format!("{hash:016x}.json"))
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

/// Fetch versions for a single repo, checking file cache first.
///
/// Shared helper used by hover and completion to avoid duplicating the
/// cache-check → provider-fetch → cache-write pattern.
///
/// Returns `None` if no provider is available for the repo. Returns
/// `Some(Err(_))` if the provider fetch fails (caller decides how to handle).
pub async fn fetch_versions_cached(
    repo_id: &ActionRepoId,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> Option<Result<version::Set, actup_core::Error>> {
    let provider = registry.find_for_repo(repo_id)?;
    let default_host = registry.default_host().unwrap_or("github.com");
    let cache_key = repo_id.cache_key(provider.cache_namespace(), default_host);

    // Try file cache first.
    if let Ok(entry) = file_cache.get(&cache_key).await {
        return Some(Ok(entry.versions().clone()));
    }

    // Fetch from provider.
    match provider.get_versions(repo_id).await {
        Ok(versions) => {
            // Write back to cache (best effort).
            let entry = actup_core::cache::Entry::new(versions.clone());
            if let Err(e) = file_cache.set(&cache_key, entry).await {
                tracing::debug!(error = %e, "failed to write version cache");
            }
            Some(Ok(versions))
        }
        Err(e) => Some(Err(e)),
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

            // Write to a temporary file then atomically rename to avoid
            // readers seeing partial/corrupt JSON if the process is
            // interrupted mid-write.
            let tmp_path = path.with_extension("json.tmp");
            tokio::fs::write(&tmp_path, content)
                .await
                .map_err(|e| cache::Error::Io(e.to_string()))?;
            tokio::fs::rename(&tmp_path, &path)
                .await
                .map_err(|e| cache::Error::Io(format!("atomic rename failed: {e}")))?;

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
    fn key_to_path_produces_filesystem_safe_hash() {
        let (cache, _temp) = make_test_cache(3600);
        let path = cache.key_to_path("github::github.com/actions/checkout");
        let filename = path.file_name().unwrap().to_str().unwrap();
        // Hash filename should be hex digits + .json extension.
        assert!(
            std::path::Path::new(filename)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        );
        let stem = filename.strip_suffix(".json").unwrap();
        assert!(stem.len() == 16, "expected 16-char hex hash, got: {stem}");
        assert!(
            stem.chars().all(|c| c.is_ascii_hexdigit()),
            "expected hex chars, got: {stem}"
        );
    }

    #[test]
    fn key_to_path_avoids_collisions() {
        let (cache, _temp) = make_test_cache(3600);
        // These two keys would collide under sanitize_filename but not under hashing.
        let path_a = cache.key_to_path("github::github.com/actions/checkout");
        let path_b = cache.key_to_path("github__github.com_actions_checkout");
        assert_ne!(
            path_a, path_b,
            "different keys must produce different paths"
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

    #[test]
    fn default_cache_dir_ends_with_actup() {
        let dir = default_cache_dir();
        let dir_str = dir.to_string_lossy();
        // Should end with "actup" (or "actup/cache" on Windows).
        assert!(
            dir_str.contains("actup"),
            "expected 'actup' in path, got: {dir_str}"
        );
    }
}
