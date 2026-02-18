//! Tag processing utilities
//!
//! Provides the `TagLike` trait and generic tag processing logic
//! shared across all provider implementations.

use actup_core::version;
use std::collections::HashMap;
use std::sync::Arc;

/// Trait for provider-specific tag types
///
/// Implement this trait for your provider's tag response type to use
/// the generic [`process_tags`] function.
///
/// # Example
///
/// ```ignore
/// impl TagLike for GitHubTag {
///     fn name(&self) -> &str {
///         &self.name
///     }
///     fn sha(&self) -> &str {
///         &self.commit.sha
///     }
/// }
/// ```
pub trait TagLike {
    /// Returns the tag name (e.g., "v1.0.0", "v2", "latest")
    fn name(&self) -> &str;

    /// Returns the commit SHA this tag points to
    fn sha(&self) -> &str;
}

/// Process tags into a `version::Set`, filtering and sorting
///
/// This function takes any iterator of items implementing [`TagLike`]
/// and produces a [`version::Set`] containing:
/// - All semver tags sorted in descending order
/// - Latest version overall (excluding prereleases)
/// - Latest version per major (excluding prereleases)
/// - Major-only tags (v1, v2, etc.)
///
/// # Arguments
///
/// * `tags` - Iterator of tag-like items to process
///
/// # Returns
///
/// A [`version::Set`] containing the processed version information.
#[must_use]
pub fn process_tags<T: TagLike>(tags: impl IntoIterator<Item = T>) -> version::Set {
    let mut semver_tags: Vec<Arc<version::Info>> = Vec::new();
    let mut latest_per_major: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut major_tags: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut latest: Option<Arc<version::Info>> = None;

    for tag in tags {
        let version = version::Version::parse(tag.name());

        // Only create Info for semver tags to avoid allocations for non-semver
        if let version::Version::Semver {
            major, prerelease, ..
        } = &version
        {
            // Wrap in Arc for cheap cloning into multiple collections
            let info = Arc::new(version::Info::new(version.clone(), tag.sha()));
            // Always add to semver_tags (prereleases are filtered at query time)
            semver_tags.push(Arc::clone(&info));

            // Track major-only tags (v1, v2, v6 - no minor/patch)
            if version.is_major_only() {
                major_tags.insert(*major, Arc::clone(&info));
            }

            // Only update "latest" trackers for non-prerelease versions
            // (prereleases are filtered at query time via allow_prerelease flag)
            if prerelease.is_none() {
                // Determine if current `version` is dominated by existing trackers.
                // `dominated_by_latest`: true if global `latest` >= this `version`
                // `dominated_by_major`: true if `latest_per_major[major]` >= this `version`
                let dominated_by_latest = latest.as_ref().is_some_and(|l| l.version() >= &version);
                let dominated_by_major = latest_per_major
                    .get(major)
                    .is_some_and(|c| c.version() >= &version);

                // Two-way update logic for `latest` and `latest_per_major`:
                //
                // 1. Neither dominated: `info` is the new overall latest AND new latest for
                //    this `major`. Update both `latest` and `latest_per_major[major]`.
                //
                // 2. `dominated_by_latest` but NOT `dominated_by_major`: Global latest is
                //    higher (different major), but this is a new latest for THIS major.
                //    Only update `latest_per_major[major]`.
                //
                // 3. Both dominated: `info` is older than existing trackers; no updates.
                //
                // Note: The case where `dominated_by_major` is true but `dominated_by_latest`
                // is false is unreachable because `latest` is always >= any `latest_per_major`
                // value (global max >= per-major max).
                if !dominated_by_latest && !dominated_by_major {
                    latest_per_major.insert(*major, Arc::clone(&info));
                    latest = Some(info);
                } else if !dominated_by_major {
                    latest_per_major.insert(*major, Arc::clone(&info));
                }
            }
        }
    }

    // Sort semver tags descending
    semver_tags.sort_by(|a, b| b.version().cmp(a.version()));

    version::Set::build(semver_tags, latest_per_major, major_tags, latest)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Simple tag type for testing
    struct TestTag {
        name: String,
        sha: String,
    }

    impl TestTag {
        fn new(name: &str, sha: &str) -> Self {
            Self {
                name: name.to_string(),
                sha: sha.to_string(),
            }
        }
    }

    impl TagLike for TestTag {
        fn name(&self) -> &str {
            &self.name
        }
        fn sha(&self) -> &str {
            &self.sha
        }
    }

    #[test]
    fn test_process_tags_empty() {
        let tags: Vec<TestTag> = vec![];
        let result = process_tags(tags);
        assert!(result.semver_tags().is_empty());
        assert!(result.latest().is_none());
        assert!(result.latest_per_major().is_empty());
    }

    #[test]
    fn test_process_tags_single_version() {
        let tags = vec![TestTag::new("v1.0.0", "aaa111")];
        let result = process_tags(tags);

        assert_eq!(result.semver_tags().len(), 1);
        assert!(result.latest().is_some());
        assert_eq!(
            result.latest().as_ref().unwrap().version().to_string(),
            "v1.0.0"
        );
        assert_eq!(result.latest_per_major().len(), 1);
        assert!(result.latest_per_major().contains_key(&1));
    }

    #[test]
    fn test_process_tags_multiple_versions_sorted() {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("v2.0.0", "bbb"),
            TestTag::new("v1.5.0", "ccc"),
        ];
        let result = process_tags(tags);

        // Should be sorted descending
        assert_eq!(result.semver_tags().len(), 3);
        assert_eq!(result.semver_tags()[0].version().to_string(), "v2.0.0");
        assert_eq!(result.semver_tags()[1].version().to_string(), "v1.5.0");
        assert_eq!(result.semver_tags()[2].version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_latest_tracking() {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("v2.0.0", "bbb"),
            TestTag::new("v3.0.0", "ccc"),
        ];
        let result = process_tags(tags);

        assert!(result.latest().is_some());
        assert_eq!(result.latest().unwrap().version().to_string(), "v3.0.0");
    }

    #[test]
    fn test_process_tags_latest_per_major() {
        let tags = vec![
            TestTag::new("v1.0.0", "a"),
            TestTag::new("v1.1.0", "b"),
            TestTag::new("v1.2.0", "c"),
            TestTag::new("v2.0.0", "d"),
            TestTag::new("v2.1.0", "e"),
        ];
        let result = process_tags(tags);

        assert_eq!(result.latest_per_major().len(), 2);
        assert_eq!(
            result
                .latest_per_major()
                .get(&1)
                .unwrap()
                .version()
                .to_string(),
            "v1.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .unwrap()
                .version()
                .to_string(),
            "v2.1.0"
        );
    }

    #[test]
    fn test_process_tags_includes_prereleases_in_tags() {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("v2.0.0-beta.1", "bbb"),
            TestTag::new("v2.0.0-rc.1", "ccc"),
        ];
        let result = process_tags(tags);

        // Prereleases are included in semver_tags (filtered at query time)
        assert_eq!(result.semver_tags().len(), 3);
        // But "latest" only tracks stable releases
        assert_eq!(result.latest().unwrap().version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_non_semver_ignored() {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("latest", "bbb"),
            TestTag::new("main", "ccc"),
        ];
        let result = process_tags(tags);

        // Non-semver should be ignored
        assert_eq!(result.semver_tags().len(), 1);
        assert_eq!(result.latest().unwrap().version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_handles_major_only_tags() {
        let tags = vec![
            TestTag::new("v1.0.0", "sha_v1_0_0"),
            TestTag::new("v2.1.0", "sha_v2_1_0"),
            TestTag::new("v2.2.0", "sha_v2_2_0"),
            TestTag::new("v2", "sha_v2_major"),
        ];
        let result = process_tags(tags);

        // All semver tags should be present (including major-only tag "v2")
        assert_eq!(result.semver_tags().len(), 4);

        // Latest overall should be the highest stable version (v2.2.0)
        assert_eq!(result.latest().unwrap().version().to_string(), "v2.2.0");
        assert_eq!(result.latest().unwrap().sha(), "sha_v2_2_0");

        // major_tags should contain the major-only tag "v2"
        assert_eq!(result.major_tags().len(), 1);
        assert!(result.major_tags().contains_key(&2));
        assert_eq!(
            result.major_tags().get(&2).unwrap().version().to_string(),
            "v2"
        );
        assert_eq!(result.major_tags().get(&2).unwrap().sha(), "sha_v2_major");

        // latest_per_major should track the highest stable version for major 2 (v2.2.0)
        assert_eq!(result.latest_per_major().len(), 2);
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .unwrap()
                .version()
                .to_string(),
            "v2.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&1)
                .unwrap()
                .version()
                .to_string(),
            "v1.0.0"
        );
    }
}
