//! Tag processing utilities
//!
//! Provides the `TagLike` trait and generic tag processing logic
//! shared across all provider implementations.

use actup_core::version;

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
/// - Non-semver tag names with their commits
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
    version::Set::from_tags(
        tags.into_iter()
            .map(|tag| (tag.name().to_string(), tag.sha().to_string())),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

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
        assert_eq!(result.semver_tags(), []);
        assert!(result.latest().is_none());
        assert!(result.latest_per_major().is_empty());
    }

    #[test]
    fn test_process_tags_single_version() -> TestResult {
        let tags = vec![TestTag::new("v1.0.0", "aaa111")];
        let result = process_tags(tags);

        assert_eq!(result.semver_tags().len(), 1);
        assert!(result.latest().is_some());
        assert_eq!(
            result
                .latest()
                .as_ref()
                .ok_or("`result.latest().as_ref()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        assert_eq!(result.latest_per_major().len(), 1);
        assert!(result.latest_per_major().contains_key(&1));
        Ok(())
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
    fn test_process_tags_latest_tracking() -> TestResult {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("v2.0.0", "bbb"),
            TestTag::new("v3.0.0", "ccc"),
        ];
        let result = process_tags(tags);

        assert!(result.latest().is_some());
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v3.0.0"
        );
        Ok(())
    }

    #[test]
    fn test_process_tags_latest_per_major() -> TestResult {
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
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v1.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v2.1.0"
        );
        Ok(())
    }

    #[test]
    fn test_process_tags_includes_prereleases_in_tags() -> TestResult {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("v2.0.0-beta.1", "bbb"),
            TestTag::new("v2.0.0-rc.1", "ccc"),
        ];
        let result = process_tags(tags);

        // Prereleases are included in semver_tags (filtered at query time)
        assert_eq!(result.semver_tags().len(), 3);
        // But "latest" only tracks stable releases
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        Ok(())
    }

    #[test]
    fn process_tags_orders_by_semver_precedence() -> Result<(), Box<dyn std::error::Error>> {
        let tags = vec![
            TestTag::new("v2.0.0-rc.2", "a"),
            TestTag::new("v2.0.0-rc.10", "b"),
            TestTag::new("v1.0.0", "c"),
            TestTag::new("v1.0.1+build.7", "d"),
            TestTag::new("2024-01-15", "e"),
            TestTag::new("v2.0.0-beta.11", "f"),
        ];
        let result = process_tags(tags);

        let order: Vec<&str> = result
            .semver_tags()
            .iter()
            .map(|t| t.version().as_str())
            .collect();
        assert_eq!(
            order,
            [
                "v2.0.0-rc.10",
                "v2.0.0-rc.2",
                "v2.0.0-beta.11",
                "v1.0.1+build.7",
                "v1.0.0"
            ]
        );
        let latest = result.latest().ok_or("expected latest")?;
        assert_eq!(latest.version().as_str(), "v1.0.1+build.7");
        assert_eq!(latest.sha(), "d");
        Ok(())
    }

    #[test]
    fn process_tags_shares_one_info_per_tag() -> Result<(), Box<dyn std::error::Error>> {
        let result = process_tags(vec![
            TestTag::new("v1.0.0", "a"),
            TestTag::new("v2", "b"),
            TestTag::new("v2.1.0", "b"),
        ]);

        let latest = result.latest().ok_or("expected latest")?;
        let per_major = result
            .latest_per_major()
            .get(&2)
            .ok_or("expected major 2")?;
        let listed = result
            .semver_tags()
            .iter()
            .find(|t| t.version().as_str() == "v2.1.0")
            .ok_or("expected v2.1.0")?;
        let indexed = result
            .tags_on("b")
            .iter()
            .find(|t| t.version().as_str() == "v2.1.0")
            .ok_or("expected v2.1.0 on b")?;
        assert!(Arc::ptr_eq(latest, per_major));
        assert!(Arc::ptr_eq(latest, listed));
        assert!(Arc::ptr_eq(latest, indexed));

        let major = result.major_tags().get(&2).ok_or("expected v2")?;
        let major_listed = result
            .semver_tags()
            .iter()
            .find(|t| t.version().as_str() == "v2")
            .ok_or("expected v2 listed")?;
        let major_indexed = result
            .tags_on("b")
            .iter()
            .find(|t| t.version().as_str() == "v2")
            .ok_or("expected v2 on b")?;
        assert!(Arc::ptr_eq(major, major_listed));
        assert!(Arc::ptr_eq(major, major_indexed));
        Ok(())
    }

    #[test]
    fn test_process_tags_non_semver_ignored() -> TestResult {
        let tags = vec![
            TestTag::new("v1.0.0", "aaa"),
            TestTag::new("latest", "bbb"),
            TestTag::new("main", "ccc"),
        ];
        let result = process_tags(tags);

        assert_eq!(result.semver_tags().len(), 1);
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        assert_eq!(result.other_tag_sha("latest"), Some("bbb"));
        assert_eq!(result.other_tag_sha("main"), Some("ccc"));
        assert_eq!(result.other_tag_sha("v1.0.0"), None);
        assert_eq!(result.listing(), version::Listing::Partial);
        Ok(())
    }

    #[test]
    fn test_process_tags_handles_major_only_tags() -> TestResult {
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
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v2.2.0"
        );
        assert_eq!(
            result.latest().ok_or("`result.latest()` is None")?.sha(),
            "sha_v2_2_0"
        );

        // major_tags should contain the major-only tag "v2"
        assert_eq!(result.major_tags().len(), 1);
        assert!(result.major_tags().contains_key(&2));
        assert_eq!(
            result
                .major_tags()
                .get(&2)
                .ok_or("`result.major_tags().get(..)` is None")?
                .version()
                .to_string(),
            "v2"
        );
        assert_eq!(
            result
                .major_tags()
                .get(&2)
                .ok_or("`result.major_tags().get(..)` is None")?
                .sha(),
            "sha_v2_major"
        );

        // latest_per_major should track the highest stable version for major 2 (v2.2.0)
        assert_eq!(result.latest_per_major().len(), 2);
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v2.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&1)
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        Ok(())
    }
}
