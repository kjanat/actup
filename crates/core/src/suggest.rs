//! Closest-tag suggestions for refs that name no tag.

use std::sync::Arc;

use crate::version::{Info, Set, Version, rank};

const MAX_EDITS: usize = 2;

enum Shape {
    Partial,
    Full,
    Other,
}

fn shape(ref_name: &str) -> Shape {
    let core = ref_name.strip_prefix('v').unwrap_or(ref_name);
    let (numbers, prerelease) = core
        .split_once('-')
        .map_or((core, None), |(numbers, pre)| (numbers, Some(pre)));
    let parts: Vec<&str> = numbers.split('.').collect();
    let numeric = parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let prerelease_ok = prerelease.is_none_or(|pre| {
        !pre.is_empty()
            && pre
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    match (numeric && prerelease_ok, parts.len(), prerelease) {
        (true, 1 | 2, None) => Shape::Partial,
        (true, 3, _) => Shape::Full,
        _ => Shape::Other,
    }
}

/// Return the listed tag closest to `ref_name`, counting prerelease tags only when `allow_prerelease` is set.
#[must_use]
pub fn closest_tag(ref_name: &str, versions: &Set, allow_prerelease: bool) -> Option<Arc<Info>> {
    let tags = || {
        versions
            .semver_tags()
            .iter()
            .filter(move |tag| allow_prerelease || !tag.version().is_prerelease())
    };
    let full_only = match shape(ref_name) {
        Shape::Full => {
            let wanted = Version::parse(ref_name);
            return versions
                .semver_tags()
                .iter()
                .filter(|tag| tag.version().cmp(&wanted).is_eq())
                .filter(|tag| tag.version().as_str() != ref_name)
                .max_by(|a, b| rank(a.version(), b.version()))
                .map(Arc::clone);
        }
        Shape::Partial => {
            let wanted = Version::parse(ref_name);
            let completes = tags()
                .filter(|tag| {
                    let have = tag.version();
                    have.major() == wanted.major()
                        && wanted.minor().is_none_or(|m| have.minor() == Some(m))
                })
                .max_by(|a, b| rank(a.version(), b.version()));
            if let Some(tag) = completes {
                return Some(Arc::clone(tag));
            }
            true
        }
        Shape::Other => false,
    };
    tags()
        .filter(|tag| !full_only || tag.version().patch().is_some())
        .map(|tag| (strsim::levenshtein(ref_name, tag.version().as_str()), tag))
        .filter(|(edits, _)| (1..=MAX_EDITS).contains(edits))
        .min_by(|(a, x), (b, y)| a.cmp(b).then_with(|| rank(y.version(), x.version())))
        .map(|(_, tag)| Arc::clone(tag))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn set(tags: &[&str]) -> Set {
        let infos = tags
            .iter()
            .enumerate()
            .map(|(i, tag)| Arc::new(Info::new(Version::parse(tag), format!("{i:040}"))))
            .collect();
        Set::build(infos, HashMap::new(), HashMap::new(), None)
    }

    fn suggest(ref_name: &str, tags: &[&str]) -> Option<String> {
        closest_tag(ref_name, &set(tags), false).map(|t| t.version().to_string())
    }

    #[test]
    fn prerelease_tags_count_when_the_policy_allows_them() {
        let tags = set(&["v4.2.0", "v4.2.1-rc.1"]);
        let pick = |allow| closest_tag("v4.2", &tags, allow).map(|t| t.version().to_string());
        assert_eq!(pick(false).as_deref(), Some("v4.2.0"));
        assert_eq!(pick(true).as_deref(), Some("v4.2.1-rc.1"));
        assert_eq!(
            closest_tag("v4.2.1_rc.1", &tags, true).map(|t| t.version().to_string()),
            Some("v4.2.1-rc.1".to_string())
        );
        assert_eq!(closest_tag("v4.2.1_rc.1", &tags, false), None);
    }

    #[test]
    fn equal_versions_pick_the_same_tag_in_any_order() {
        let forward = ["v4.2", "v4.2.0", "v4.2.0+build.1"];
        let backward = ["v4.2.0+build.1", "v4.2.0", "v4.2"];
        assert_eq!(suggest("v4", &forward).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4", &backward).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4.2.O", &forward).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4.2.O", &backward).as_deref(), Some("v4.2.0"));
    }

    #[test]
    fn partial_versions_name_the_highest_matching_stable_tag() {
        let tags = [
            "v4.1.9",
            "v4.2.0",
            "v4.2.3",
            "v4.2.4-rc.1",
            "v4.3.0",
            "v5.0.0",
        ];
        assert_eq!(suggest("v4.2", &tags).as_deref(), Some("v4.2.3"));
        assert_eq!(suggest("4.2", &tags).as_deref(), Some("v4.2.3"));
        assert_eq!(suggest("v4", &tags).as_deref(), Some("v4.3.0"));
    }

    #[test]
    fn missing_full_versions_get_no_suggestion() {
        let tags = ["v4.2.0", "v4.2.1"];
        assert_eq!(suggest("v4.2.2", &tags), None);
        assert_eq!(suggest("v4.2.1-rc.1", &tags), None);
    }

    #[test]
    fn full_versions_name_the_listed_tag_of_equal_version() {
        assert_eq!(suggest("4.2.0", &["v4.2.0"]).as_deref(), Some("v4.2.0"));
        assert_eq!(
            suggest("4.2.0", &["v4.2.0+build.1", "v4.2.0"]).as_deref(),
            Some("v4.2.0")
        );
        assert_eq!(suggest("v4.2.0", &["4.2.0"]).as_deref(), Some("4.2.0"));
    }

    #[test]
    fn partial_versions_fall_back_only_to_full_tags() {
        let tags = ["v1", "v6", "v6.0.0", "v4.2", "v4.2.0"];
        assert_eq!(suggest("v7", &tags), None);
        assert_eq!(suggest("v4.3", &tags), None);
        assert_eq!(
            suggest("v42.0", &["v4.2", "v4.2.0"]).as_deref(),
            Some("v4.2.0")
        );
    }

    #[test]
    fn other_refs_name_a_tag_within_two_edits() {
        let tags = ["v3.9.0", "v4.1.0", "v4.2.0"];
        assert_eq!(suggest("v42.0", &tags).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4.2.O", &tags).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4.2.0.", &tags).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("vv4.1.0", &tags).as_deref(), Some("v4.1.0"));
        assert_eq!(suggest("v4.2.0-", &tags).as_deref(), Some("v4.2.0"));
    }

    #[test]
    fn the_fewest_edits_win_and_a_tie_goes_to_the_higher_version() {
        assert_eq!(
            suggest("v1.x.0", &["v1.0.0", "v1.2.0"]).as_deref(),
            Some("v1.2.0")
        );
        assert_eq!(
            suggest("v1.2.0x", &["v1.2.0", "v1.9.9"]).as_deref(),
            Some("v1.2.0")
        );
    }

    #[test]
    fn distant_refs_get_no_suggestion() {
        let tags = ["v4.1.0", "v4.2.0", "v5.0.0-rc.1"];
        assert_eq!(suggest("main", &tags), None);
        assert_eq!(suggest("v5", &tags), None);
        assert_eq!(suggest("release-4.2.0", &tags), None);
        assert_eq!(suggest("v4.2", &[]), None);
    }
}
