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
mod tests {
    use super::*;

    #[test]
    fn test_cache_entry_expiry() {
        let entry = CacheEntry::new(VersionSet::default());

        // Should not be expired immediately with 1 hour TTL
        assert!(!entry.is_expired(Duration::from_secs(3600)));

        // Should be expired with 0 TTL
        assert!(entry.is_expired(Duration::ZERO));
    }
}
