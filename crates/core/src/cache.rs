//! Cache interface for version information
//!
//! This module defines the caching interface. The actual storage
//! implementation is left to the caller (file-based, in-memory, etc.).

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{
    ActionRepoId,
    clock::{Clock, SystemClock},
    compat::{BoxFuture, MaybeSend, MaybeSync},
    version,
};

/// A cached entry with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// The cached version set
    versions: version::Set,

    /// When this entry was stored
    stored_at: SystemTime,

    /// Opaque validators the provider sends with conditional requests, keyed by request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    validators: BTreeMap<String, String>,
}

impl Entry {
    /// Create a new cache entry (uses [`SystemClock`]).
    #[must_use]
    pub fn new(versions: version::Set) -> Self {
        Self::with_clock(versions, &SystemClock)
    }

    /// Create a new cache entry using the provided [`Clock`].
    #[must_use]
    pub fn with_clock(versions: version::Set, clock: &dyn Clock) -> Self {
        Self {
            versions,
            stored_at: clock.now(),
            validators: BTreeMap::new(),
        }
    }

    /// Check if this entry is expired based on TTL
    #[must_use]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.stored_at
            .elapsed()
            .map_or(true, |elapsed| elapsed > ttl)
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

    /// Get the conditional request validators
    #[must_use]
    pub const fn validators(&self) -> &BTreeMap<String, String> {
        &self.validators
    }

    /// Builder-style setter for the conditional request validators
    #[must_use]
    pub fn with_validators(mut self, validators: BTreeMap<String, String>) -> Self {
        self.validators = validators;
        self
    }

    /// This entry stored again now, with its versions and validators.
    #[must_use]
    pub fn renewed(self) -> Self {
        Self {
            stored_at: SystemClock.now(),
            ..self
        }
    }

    /// This entry's versions marked stale, when `error` is a rate limit or a network failure.
    #[must_use]
    pub fn stale_versions(&self, error: &crate::Error) -> Option<version::Set> {
        let staleness = Staleness::after(error, self.age().unwrap_or_default())?;
        Some(self.versions.clone().into_stale(staleness))
    }
}

/// A cache read: an entry within its TTL, or one past it.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// The entry is within its TTL.
    Fresh(Entry),
    /// The entry is past its TTL.
    Expired(Entry),
}

/// Why a provider could not refresh an expired entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// The provider rate limited the request.
    RateLimited,
    /// The request failed on the network.
    Network,
}

/// The age of a version set read from an expired entry, and why it was not refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staleness {
    age: Duration,
    reason: StaleReason,
    retry_after: Option<Duration>,
}

impl Staleness {
    /// The staleness of an entry aged `age` after `error`, for a rate limit or a network failure.
    #[must_use]
    pub const fn after(error: &crate::Error, age: Duration) -> Option<Self> {
        let (reason, retry_after) = match error {
            crate::Error::RateLimited { retry_after, .. } => {
                (StaleReason::RateLimited, *retry_after)
            }
            crate::Error::Network { .. } => (StaleReason::Network, None),
            _ => return None,
        };
        Some(Self {
            age,
            reason,
            retry_after,
        })
    }

    /// How long ago the entry was stored.
    #[must_use]
    pub const fn age(&self) -> Duration {
        self.age
    }

    /// Why the entry was not refreshed.
    #[must_use]
    pub const fn reason(&self) -> StaleReason {
        self.reason
    }

    /// When the provider accepts requests again, if it said so.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl Serialize for Staleness {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let fields = if self.retry_after.is_some() { 3 } else { 2 };
        let mut state = serializer.serialize_struct("Staleness", fields)?;
        state.serialize_field("age_secs", &self.age.as_secs())?;
        state.serialize_field("reason", &self.reason)?;
        if let Some(retry_after) = self.retry_after {
            state.serialize_field("retry_after_secs", &retry_after.as_secs())?;
        }
        state.end()
    }
}

impl std::fmt::Display for Staleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.reason {
            StaleReason::RateLimited => "rate limited",
            StaleReason::Network => "network error",
        };
        write!(f, "fetched {} ago, {reason}", span(self.age))?;
        if let Some(retry_after) = self.retry_after {
            write!(f, ", resets in {}", span(retry_after))?;
        }
        Ok(())
    }
}

fn span(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..172_800 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// An [`Entry`] with the key it is stored under, as a disk cache writes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    key: String,
    entry: Entry,
}

impl Record {
    /// Pair `entry` with the `key` it is stored under.
    #[must_use]
    pub fn new(key: &str, entry: Entry) -> Self {
        Self {
            key: key.to_owned(),
            entry,
        }
    }

    /// Return the entry when the record was stored under `key`.
    #[must_use]
    pub fn entry_for(self, key: &str) -> Option<Entry> {
        (self.key == key).then_some(self.entry)
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
pub trait Cache: MaybeSend + MaybeSync {
    /// Get a cached entry by key
    fn get(&self, key: &str) -> BoxFuture<'_, Result<Entry>>;

    /// Get a cached entry by key, past its TTL too
    fn lookup(&self, key: &str) -> BoxFuture<'_, Result<Lookup>> {
        let entry = self.get(key);
        Box::pin(async move { entry.await.map(Lookup::Fresh) })
    }

    /// Store an entry in the cache
    fn set(&self, key: &str, entry: Entry) -> BoxFuture<'_, Result<()>>;

    /// Remove an entry from the cache
    fn remove(&self, key: &str) -> BoxFuture<'_, Result<()>>;

    /// Clear all cache entries
    fn clear(&self) -> BoxFuture<'_, Result<()>>;

    /// Check if caching is enabled
    fn is_enabled(&self) -> bool {
        true
    }
}

/// A no-op cache that doesn't store anything
#[derive(Debug, Default)]
pub struct Noop;

impl Cache for Noop {
    fn get(&self, _key: &str) -> BoxFuture<'_, Result<Entry>> {
        Box::pin(async { Err(Error::Disabled) })
    }

    fn set(&self, _key: &str, _entry: Entry) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, _key: &str) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn is_enabled(&self) -> bool {
        false
    }
}

/// The warning logged when the disk cache is off because no cache directory is known.
pub const NO_DIR_WARNING: &str = "no OS cache directory, so the disk cache is off for this run; set `cache.path` in the user config or `ACTUP_CACHE__PATH`";

/// Return the OS cache directory for actup, or `None` when the OS reports none.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    let base = dirs::cache_dir()?.join("actup");
    #[cfg(windows)]
    let base = base.join("cache");
    Some(base)
}

/// Return the directory for the disk cache: `path` when set, else [`default_dir`].
#[must_use]
pub fn dir(path: Option<&std::path::Path>) -> Option<PathBuf> {
    path.map(std::path::Path::to_path_buf).or_else(default_dir)
}

/// Return the disk cache file name for `key`: its 64-bit FNV-1a hash in hex, then `.json`.
#[must_use]
pub fn entry_file_name(key: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let hash = key.bytes().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    });
    format!("{hash:016x}.json")
}

/// Return whether `name` has the form [`entry_file_name`] writes.
#[must_use]
pub fn is_entry_file_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(|stem| {
        stem.len() == 16 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
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

    #[test]
    fn entry_file_names_are_hashed_and_recognized() {
        let name = entry_file_name("github::github.com/actions/checkout");
        assert!(is_entry_file_name(&name), "{name}");
        assert_eq!(name.len(), 21);
        assert_ne!(name, entry_file_name("github__github.com_actions_checkout"));
        assert_eq!(name, entry_file_name("github::github.com/actions/checkout"));
        for foreign in [
            "package.json",
            "tsconfig.json",
            "0123456789ABCDEF.json",
            "0123456789abcde.json",
            "0123456789abcdef.toml",
            "0123456789abcdef",
        ] {
            assert!(!is_entry_file_name(foreign), "{foreign}");
        }
    }

    #[test]
    fn record_returns_its_entry_only_for_its_key() -> serde_json::Result<()> {
        let record = Record::new(
            "github::github.com/a/b",
            Entry::new(version::Set::default()),
        );
        let json = serde_json::to_string(&record)?;
        let stored: Record = serde_json::from_str(&json)?;
        assert!(stored.entry_for("github::github.com/a/b").is_some());
        let stored: Record = serde_json::from_str(&json)?;
        assert!(stored.entry_for("github::github.com/a/c").is_none());
        Ok(())
    }

    #[test]
    fn dir_prefers_the_configured_path() {
        let path = std::path::Path::new("/custom/cache");
        assert_eq!(dir(Some(path)), Some(path.to_path_buf()));
        assert_eq!(dir(None), default_dir());
    }

    // Entry tests

    #[test]
    fn test_cache_entry_new() {
        let entry = Entry::new(version::Set::default());
        assert!(entry.validators().is_empty());
        // stored_at should be set (age is computable)
        assert!(entry.age().is_some());
    }

    #[test]
    fn test_cache_entry_expiry() {
        let entry = Entry::new(version::Set::default());

        // Should not be expired immediately with 1 hour TTL
        assert!(!entry.is_expired(Duration::from_hours(1)));

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

    fn rate_limited(retry_after: Option<Duration>) -> crate::Error {
        crate::Error::RateLimited {
            provider: "github".into(),
            retry_after,
        }
    }

    #[test]
    fn staleness_follows_rate_limits_and_network_failures_only() {
        let age = Duration::from_hours(26);
        let network = crate::Error::Network {
            operation: "GET".into(),
            message: "connection failed".into(),
            source: None,
        };
        let auth = crate::Error::Auth {
            provider: "github".into(),
            reason: "bad token".into(),
            source: None,
        };
        let missing = crate::Error::NotFound {
            owner: "o".into(),
            repo: "r".into(),
            host: None,
        };

        let limited = Staleness::after(&rate_limited(Some(Duration::from_mins(12))), age);
        assert_eq!(
            limited.map(|s| (s.age(), s.reason(), s.retry_after())),
            Some((age, StaleReason::RateLimited, Some(Duration::from_mins(12))))
        );
        assert_eq!(
            Staleness::after(&network, age).map(|s| (s.reason(), s.retry_after())),
            Some((StaleReason::Network, None))
        );
        assert_eq!(Staleness::after(&auth, age), None);
        assert_eq!(Staleness::after(&missing, age), None);
    }

    #[test]
    fn staleness_reads_its_age_reason_and_reset() {
        let show = |error: &crate::Error, age: Duration| {
            Staleness::after(error, age).map(|s| s.to_string())
        };
        assert_eq!(
            show(
                &rate_limited(Some(Duration::from_mins(12))),
                Duration::from_hours(26)
            ),
            Some("fetched 26h ago, rate limited, resets in 12m".into())
        );
        assert_eq!(
            show(
                &rate_limited(Some(Duration::from_secs(59))),
                Duration::from_hours(72)
            ),
            Some("fetched 3d ago, rate limited, resets in 59s".into())
        );
        assert_eq!(
            show(
                &crate::Error::Network {
                    operation: String::new(),
                    message: String::new(),
                    source: None,
                },
                Duration::from_mins(90)
            ),
            Some("fetched 1h ago, network error".into())
        );
    }

    #[test]
    fn stale_versions_marks_a_copy_and_renewed_restamps_the_entry()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct Past(SystemTime);
        impl Clock for Past {
            fn now(&self) -> SystemTime {
                self.0
            }
        }
        let then = SystemTime::now()
            .checked_sub(Duration::from_hours(26))
            .ok_or("clock before epoch")?;
        let validators = BTreeMap::from([("tags".to_string(), "\"t\"".to_string())]);
        let entry = Entry::with_clock(version::Set::default(), &Past(then))
            .with_validators(validators.clone());

        let stale = entry
            .stale_versions(&rate_limited(None))
            .ok_or("no stale versions")?
            .stale()
            .ok_or("not marked")?;
        assert!(stale.age() >= Duration::from_hours(26));
        assert_eq!(entry.versions().stale(), None);
        assert!(
            entry
                .stale_versions(&crate::Error::Internal {
                    context: String::new(),
                    message: String::new(),
                    source: None,
                })
                .is_none()
        );

        assert!(entry.is_expired(Duration::from_hours(1)));
        let renewed = entry.renewed();
        assert!(!renewed.is_expired(Duration::from_hours(1)));
        assert_eq!(renewed.validators(), &validators);
        Ok(())
    }

    #[test]
    fn validators_round_trip_and_stay_out_of_json_when_empty()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let bare = serde_json::to_value(Entry::new(version::Set::default()))?;
        assert!(bare.get("validators").is_none());

        let validators = BTreeMap::from([("repo".to_string(), "W/\"r\"".to_string())]);
        let entry = Entry::new(version::Set::default()).with_validators(validators.clone());
        let back: Entry = serde_json::from_str(&serde_json::to_string(&entry)?)?;
        assert_eq!(back.validators(), &validators);
        Ok(())
    }

    #[tokio::test]
    async fn default_lookup_answers_fresh_entries()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        struct One;
        impl Cache for One {
            fn get(&self, _key: &str) -> BoxFuture<'_, Result<Entry>> {
                Box::pin(async { Ok(Entry::new(version::Set::default())) })
            }
            fn set(&self, _key: &str, _entry: Entry) -> BoxFuture<'_, Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn remove(&self, _key: &str) -> BoxFuture<'_, Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn clear(&self) -> BoxFuture<'_, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        assert!(matches!(One.lookup("k").await?, Lookup::Fresh(_)));
        assert!(matches!(Noop.lookup("k").await, Err(Error::Disabled)));
        Ok(())
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
}
