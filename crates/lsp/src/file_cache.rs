//! Version lookups through the shared disk cache.

pub use actup_core::FileCache;
use actup_core::{
    ActionRepoId,
    cache::{self, Cache},
    provider, version,
};

/// Fetch versions for a single repo, checking file cache first.
///
/// Shared helper used by hover and completion to avoid duplicating the
/// cache-check → provider-fetch → cache-write pattern.
///
/// Returns `None` if no provider is available for the repo. Returns
/// `Some(Err(_))` if the provider fetch fails (caller decides how to handle).
/// A rate limit or network failure with an expired entry returns that
/// entry's versions marked stale.
pub async fn fetch_versions_cached(
    repo_id: &ActionRepoId,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> Option<Result<version::Set, actup_core::Error>> {
    let provider = registry.find_for_repo(repo_id)?;
    let version_repo = repo_id.without_subpath();
    let cache_key = version_repo.cache_key(provider.cache_namespace(), provider.host());

    let expired = match file_cache.lookup(&cache_key).await {
        Ok(cache::Lookup::Fresh(entry)) => return Some(Ok(entry.versions().clone())),
        Ok(cache::Lookup::Expired(entry)) => Some(entry),
        Err(_) => None,
    };

    // Fetch from provider.
    match provider.get_versions(&version_repo).await {
        Ok(versions) => {
            // Write back to cache (best effort).
            let entry = cache::Entry::new(versions.clone());
            if let Err(e) = file_cache.set(&cache_key, entry).await {
                tracing::debug!(error = %e, "failed to write version cache");
            }
            Some(Ok(versions))
        }
        Err(e) => Some(expired.and_then(|entry| entry.stale_versions(&e)).ok_or(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use actup_core::Provider;
    use tempfile::TempDir;

    use super::*;
    use crate::test_utils::{DUMMY_SHA, MockProvider, mock_registry, simple_version_set};

    fn make_test_cache(ttl_secs: u64) -> std::io::Result<(FileCache, TempDir)> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_secs(ttl_secs));
        Ok((cache, temp))
    }

    fn ghe_checkout() -> (ActionRepoId, String) {
        let repo = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "actions",
            "checkout",
            None,
        );
        let key = repo.cache_key(
            MockProvider::new("ghe.corp.com").cache_namespace(),
            "ghe.corp.com",
        );
        (repo, key)
    }

    #[tokio::test]
    async fn fetch_versions_cached_falls_back_to_an_expired_entry_on_a_rate_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0)?;
        let (repo, key) = ghe_checkout();
        cache
            .set(
                &key,
                cache::Entry::new(simple_version_set("v4.0.0", DUMMY_SHA)),
            )
            .await?;
        let registry = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::RateLimited {
                provider: "github".into(),
                retry_after: Some(Duration::from_mins(12)),
            },
        ));

        let versions = fetch_versions_cached(&repo, &registry, &cache)
            .await
            .ok_or("no provider")??;

        let stale = versions.stale().ok_or("not stale")?;
        assert_eq!(stale.reason(), cache::StaleReason::RateLimited);
        assert_eq!(stale.retry_after(), Some(Duration::from_mins(12)));
        assert_eq!(
            versions.latest().map(|t| t.version().to_string()),
            Some("v4.0.0".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn fetch_versions_cached_reports_errors_without_an_expired_entry_or_after_a_user_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0)?;
        let (repo, key) = ghe_checkout();
        let network = || actup_core::Error::Network {
            operation: "GET".into(),
            message: "connection failed".into(),
            source: None,
        };
        let registry = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            network(),
        ));
        assert!(matches!(
            fetch_versions_cached(&repo, &registry, &cache).await,
            Some(Err(actup_core::Error::Network { .. }))
        ));

        cache
            .set(
                &key,
                cache::Entry::new(simple_version_set("v4.0.0", DUMMY_SHA)),
            )
            .await?;
        let versions = fetch_versions_cached(&repo, &registry, &cache)
            .await
            .ok_or("no provider")??;
        assert_eq!(
            versions.stale().map(|s| s.reason()),
            Some(cache::StaleReason::Network)
        );

        let missing = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::NotFound {
                owner: "actions".into(),
                repo: "checkout".into(),
                host: None,
            },
        ));
        assert!(matches!(
            fetch_versions_cached(&repo, &missing, &cache).await,
            Some(Err(actup_core::Error::NotFound { .. }))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn fetch_versions_cached_uses_provider_host_for_cache_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        let mock_provider = MockProvider::new("ghe.corp.com");
        let namespace = mock_provider.cache_namespace().to_string();
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(mock_provider));
        let repo = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "actions",
            "checkout",
            Some("restore".to_string()),
        );
        let cached_versions = simple_version_set("v4.0.0", DUMMY_SHA);
        let cache_key = repo.without_subpath().cache_key(&namespace, "ghe.corp.com");
        let entry = cache::Entry::new(cached_versions.clone());
        cache.set(&cache_key, entry).await?;

        let result = fetch_versions_cached(&repo, &registry, &cache).await;

        let versions = result.ok_or("no provider for the repo")??;
        let latest = versions.latest().ok_or("no latest version")?;
        assert_eq!(latest.version().to_string(), "v4.0.0");
        Ok(())
    }
}
