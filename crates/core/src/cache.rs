//! Cache interface for version information
//!
//! This module defines the caching interface. The actual storage
//! implementation is left to the caller (file-based, in-memory, etc.).

use crate::{ActionRepoId, version};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

/// A cached entry with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// The cached version set
    versions: version::Set,

    /// When this entry was stored
    stored_at: SystemTime,

    /// `ETag` from the provider (for conditional requests)
    etag: Option<String>,

    /// Last-Modified header value
    last_modified: Option<String>,
}

impl Entry {
    /// Create a new cache entry
    #[must_use]
    pub fn new(versions: version::Set) -> Self {
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

    /// Get the version set
    #[must_use]
    pub const fn versions(&self) -> &version::Set {
        &self.versions
    }

    /// Get the stored timestamp
    #[must_use]
    pub const fn stored_at(&self) -> SystemTime {
        self.stored_at
    }

    /// Get the `ETag` if present
    #[must_use]
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// Get the Last-Modified header if present
    #[must_use]
    pub fn last_modified(&self) -> Option<&str> {
        self.last_modified.as_deref()
    }

    /// Builder-style setter for `ETag`
    #[must_use]
    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    /// Builder-style setter for Last-Modified
    #[must_use]
    pub fn with_last_modified(mut self, last_modified: impl Into<String>) -> Self {
        self.last_modified = Some(last_modified.into());
        self
    }
}

/// Result type for cache operations
pub type Result<T> = std::result::Result<T, Error>;

/// Cache operation errors
#[derive(Debug)]
pub enum Error {
    /// Entry not found in cache
    NotFound,
    /// IO error during cache operation
    Io(String),
    /// Serialization/deserialization error
    Serde(String),
    /// Cache is disabled
    Disabled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "cache entry not found"),
            Self::Io(e) => write!(f, "cache IO error: {e}"),
            Self::Serde(e) => write!(f, "cache serialization error: {e}"),
            Self::Disabled => write!(f, "cache is disabled"),
        }
    }
}

impl std::error::Error for Error {}

/// Trait for cache implementations
///
/// This is provider-neutral - cache keys are constructed from
/// `ActionRepoId` + provider namespace.
pub trait Cache: Send + Sync {
    /// Get a cached entry by key
    fn get(&self, key: &str) -> Pin<Box<dyn Future<Output = Result<Entry>> + Send + '_>>;

    /// Store an entry in the cache
    fn set(&self, key: &str, entry: Entry)
    -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Remove an entry from the cache
    fn remove(&self, key: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Clear all cache entries
    fn clear(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Check if caching is enabled
    fn is_enabled(&self) -> bool {
        true
    }
}

/// A no-op cache that doesn't store anything
#[derive(Debug, Default)]
pub struct Noop;

impl Cache for Noop {
    fn get(&self, _key: &str) -> Pin<Box<dyn Future<Output = Result<Entry>> + Send + '_>> {
        Box::pin(async { Err(Error::Disabled) })
    }

    fn set(
        &self,
        _key: &str,
        _entry: Entry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, _key: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn clear(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
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

    // Entry tests

    #[test]
    fn test_cache_entry_new() {
        let entry = Entry::new(version::Set::default());
        assert!(entry.etag().is_none());
        assert!(entry.last_modified().is_none());
        // stored_at should be set (age is computable)
        assert!(entry.age().is_some());
    }

    #[test]
    fn test_cache_entry_expiry() {
        let entry = Entry::new(version::Set::default());

        // Should not be expired immediately with 1 hour TTL
        assert!(!entry.is_expired(Duration::from_secs(3600)));

        // Should be expired with 0 TTL
        assert!(entry.is_expired(Duration::ZERO));
    }

    #[test]
    fn test_cache_entry_age() {
        let entry = Entry::new(version::Set::default());
        let age = entry.age();
        assert!(age.is_some());
        // Age should be small for a just-created entry.
        // Use 5s threshold to tolerate CI/clock jitter.
        assert!(
            age.unwrap() < Duration::from_secs(5),
            "entry age {age:?} exceeded 5s threshold"
        );
    }

    #[test]
    fn test_cache_entry_with_metadata() {
        let entry = Entry::new(version::Set::default())
            .with_etag("W/\"abc123\"")
            .with_last_modified("Mon, 01 Jan 2024 00:00:00 GMT");

        assert_eq!(entry.etag(), Some("W/\"abc123\""));
        assert_eq!(entry.last_modified(), Some("Mon, 01 Jan 2024 00:00:00 GMT"));
    }

    // Error tests

    #[test]
    fn test_cache_error_display() {
        assert_eq!(Error::NotFound.to_string(), "cache entry not found");
        assert_eq!(
            Error::Io("disk full".to_string()).to_string(),
            "cache IO error: disk full"
        );
        assert_eq!(
            Error::Serde("invalid JSON".to_string()).to_string(),
            "cache serialization error: invalid JSON"
        );
        assert_eq!(Error::Disabled.to_string(), "cache is disabled");
    }

    // Noop cache tests

    #[tokio::test]
    async fn test_noop_cache_get_returns_disabled() {
        let cache = Noop;
        let result = cache.get("any_key").await;
        assert!(matches!(result, Err(Error::Disabled)));
    }

    #[tokio::test]
    async fn test_noop_cache_set_succeeds() {
        let cache = Noop;
        let entry = Entry::new(version::Set::default());
        let result = cache.set("any_key", entry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_cache_remove_succeeds() {
        let cache = Noop;
        let result = cache.remove("any_key").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_cache_clear_succeeds() {
        let cache = Noop;
        let result = cache.clear().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_noop_cache_is_disabled() {
        let cache = Noop;
        assert!(!cache.is_enabled());
    }

    // build_cache_key tests

    #[test]
    fn test_build_cache_key() {
        let repo = ActionRepoId::simple("actions", "checkout");
        let key = build_cache_key("github", "github.com", &repo);
        assert!(key.contains("github"));
        assert!(key.contains("github.com"));
        assert!(key.contains("actions"));
        assert!(key.contains("checkout"));
    }

    #[test]
    fn test_build_cache_key_with_subpath() {
        let repo = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "org",
            "monorepo",
            Some("actions/build".to_string()),
        );
        let key = build_cache_key("github", "ghe.corp.com", &repo);
        assert!(key.contains("ghe.corp.com"));
        assert!(key.contains("actions/build"));
    }

    #[test]
    fn test_cache_entry_with_etag() {
        let entry = Entry::new(version::Set::default()).with_etag("W/\"abc123\"");

        assert_eq!(entry.etag(), Some("W/\"abc123\""));
        assert_eq!(entry.last_modified(), None);
    }

    #[test]
    fn test_cache_entry_with_last_modified() {
        let entry =
            Entry::new(version::Set::default()).with_last_modified("Mon, 01 Jan 2024 00:00:00 GMT");

        assert_eq!(entry.last_modified(), Some("Mon, 01 Jan 2024 00:00:00 GMT"));
        assert_eq!(entry.etag(), None);
    }

    #[test]
    fn test_cache_entry_with_both_metadata() {
        let entry = Entry::new(version::Set::default())
            .with_etag("W/\"abc123\"")
            .with_last_modified("Mon, 01 Jan 2024 00:00:00 GMT");

        assert_eq!(entry.etag(), Some("W/\"abc123\""));
        assert_eq!(entry.last_modified(), Some("Mon, 01 Jan 2024 00:00:00 GMT"));
    }
}
