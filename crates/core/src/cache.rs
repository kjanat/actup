//! Cache interface for version information
//!
//! This module defines the caching interface. The actual storage
//! implementation is left to the caller (file-based, in-memory, etc.).

use crate::{ActionRepoId, VersionSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

/// A cached entry with metadata
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The cached version set
    pub versions: VersionSet,

    /// When this entry was stored
    pub stored_at: SystemTime,

    /// `ETag` from the provider (for conditional requests)
    pub etag: Option<String>,

    /// Last-Modified header value
    pub last_modified: Option<String>,
}

impl CacheEntry {
    /// Create a new cache entry
    #[must_use]
    pub fn new(versions: VersionSet) -> Self {
        Self {
            versions,
            stored_at: SystemTime::now(),
            etag: None,
            last_modified: None,
        }
    }

    /// Check if this entry is expired based on TTL
    #[must_use]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.stored_at
            .elapsed()
            .map(|elapsed| elapsed > ttl)
            .unwrap_or(true)
    }

    /// Get the age of this entry
    #[must_use]
    pub fn age(&self) -> Option<Duration> {
        self.stored_at.elapsed().ok()
    }
}

/// Result type for cache operations
pub type CacheResult<T> = std::result::Result<T, CacheError>;

/// Cache operation errors
#[derive(Debug)]
pub enum CacheError {
    /// Entry not found in cache
    NotFound,
    /// IO error during cache operation
    Io(String),
    /// Serialization/deserialization error
    Serde(String),
    /// Cache is disabled
    Disabled,
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "cache entry not found"),
            Self::Io(e) => write!(f, "cache IO error: {e}"),
            Self::Serde(e) => write!(f, "cache serialization error: {e}"),
            Self::Disabled => write!(f, "cache is disabled"),
        }
    }
}

impl std::error::Error for CacheError {}

/// Trait for cache implementations
///
/// This is provider-neutral - cache keys are constructed from
/// `ActionRepoId` + provider namespace.
pub trait Cache: Send + Sync {
    /// Get a cached entry by key
    fn get(&self, key: &str) -> Pin<Box<dyn Future<Output = CacheResult<CacheEntry>> + Send + '_>>;

    /// Store an entry in the cache
    fn set(
        &self,
        key: &str,
        entry: CacheEntry,
    ) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>>;

    /// Remove an entry from the cache
    fn remove(&self, key: &str) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>>;

    /// Clear all cache entries
    fn clear(&self) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>>;

    /// Check if caching is enabled
    fn is_enabled(&self) -> bool {
        true
    }
}

/// A no-op cache that doesn't store anything
#[derive(Debug, Default)]
pub struct NoopCache;

impl Cache for NoopCache {
    fn get(
        &self,
        _key: &str,
    ) -> Pin<Box<dyn Future<Output = CacheResult<CacheEntry>> + Send + '_>> {
        Box::pin(async { Err(CacheError::NotFound) })
    }

    fn set(
        &self,
        _key: &str,
        _entry: CacheEntry,
    ) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, _key: &str) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn clear(&self) -> Pin<Box<dyn Future<Output = CacheResult<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn is_enabled(&self) -> bool {
        false
    }
}

/// Helper to build cache keys
#[must_use]
pub fn build_cache_key(provider_namespace: &str, host: &str, repo: &ActionRepoId) -> String {
    repo.cache_key(provider_namespace, host)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ========================================================================
    // CacheEntry tests
    // ========================================================================

    #[test]
    fn test_cache_entry_new() {
        let entry = CacheEntry::new(VersionSet::default());
        assert!(entry.etag.is_none());
        assert!(entry.last_modified.is_none());
        // stored_at should be recent
        assert!(entry.age().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn test_cache_entry_expiry() {
        let entry = CacheEntry::new(VersionSet::default());

        // Should not be expired immediately with 1 hour TTL
        assert!(!entry.is_expired(Duration::from_secs(3600)));

        // Should be expired with 0 TTL
        assert!(entry.is_expired(Duration::ZERO));
    }

    #[test]
    fn test_cache_entry_age() {
        let entry = CacheEntry::new(VersionSet::default());
        let age = entry.age();
        assert!(age.is_some());
        // Age should be very small for a just-created entry
        assert!(age.unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn test_cache_entry_with_metadata() {
        let mut entry = CacheEntry::new(VersionSet::default());
        entry.etag = Some("W/\"abc123\"".to_string());
        entry.last_modified = Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string());

        assert_eq!(entry.etag.as_deref(), Some("W/\"abc123\""));
        assert!(entry.last_modified.is_some());
    }

    // ========================================================================
    // CacheError tests
    // ========================================================================

    #[test]
    fn test_cache_error_display() {
        assert_eq!(CacheError::NotFound.to_string(), "cache entry not found");
        assert_eq!(
            CacheError::Io("disk full".to_string()).to_string(),
            "cache IO error: disk full"
        );
        assert_eq!(
            CacheError::Serde("invalid JSON".to_string()).to_string(),
            "cache serialization error: invalid JSON"
        );
        assert_eq!(CacheError::Disabled.to_string(), "cache is disabled");
    }

    // ========================================================================
    // NoopCache tests
    // ========================================================================

    #[tokio::test]
    async fn test_noop_cache_get_returns_not_found() {
        let cache = NoopCache;
        let result = cache.get("any_key").await;
        assert!(matches!(result, Err(CacheError::NotFound)));
    }

    #[tokio::test]
    async fn test_noop_cache_set_succeeds() {
        let cache = NoopCache;
        let entry = CacheEntry::new(VersionSet::default());
        let result = cache.set("any_key", entry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_cache_remove_succeeds() {
        let cache = NoopCache;
        let result = cache.remove("any_key").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_cache_clear_succeeds() {
        let cache = NoopCache;
        let result = cache.clear().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_cache_is_disabled() {
        let cache = NoopCache;
        assert!(!cache.is_enabled());
    }

    // ========================================================================
    // build_cache_key tests
    // ========================================================================

    #[test]
    fn test_build_cache_key() {
        let repo = ActionRepoId {
            host: None,
            owner: "actions".to_string(),
            repo: "checkout".to_string(),
            subpath: None,
        };
        let key = build_cache_key("github", "github.com", &repo);
        assert!(key.contains("github"));
        assert!(key.contains("github.com"));
        assert!(key.contains("actions"));
        assert!(key.contains("checkout"));
    }

    #[test]
    fn test_build_cache_key_with_subpath() {
        let repo = ActionRepoId {
            host: Some("ghe.corp.com".to_string()),
            owner: "org".to_string(),
            repo: "monorepo".to_string(),
            subpath: Some("actions/build".to_string()),
        };
        let key = build_cache_key("github", "ghe.corp.com", &repo);
        assert!(key.contains("ghe.corp.com"));
        assert!(key.contains("actions/build"));
    }
}
