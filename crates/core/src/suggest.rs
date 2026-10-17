//! Closest-tag suggestions for refs that name no tag.

use std::sync::Arc;

use crate::version::{Info, Set, Version};

const MIN_SIMILARITY: f64 = 0.9;

/// Return the highest listed tag that completes the partial version `ref_name`, or else the listed tag most similar to it.
#[must_use]
pub fn closest_tag(ref_name: &str, versions: &Set) -> Option<Arc<Info>> {
    let wanted = Version::parse(ref_name);
    let candidates = versions
        .semver_tags()
        .iter()
        .filter(|tag| wanted.is_prerelease() || !tag.version().is_prerelease())
        .filter(|tag| tag.version().as_str() != ref_name);
    let completes = |tag: &&Arc<Info>| {
        let have = tag.version();
        wanted.is_semver()
            && have.major() == wanted.major()
            && wanted.minor().is_none_or(|m| have.minor() == Some(m))
            && wanted.patch().is_none_or(|p| have.patch() == Some(p))
    };
    if let Some(tag) = candidates
        .clone()
        .filter(completes)
        .max_by(|a, b| a.version().cmp(b.version()))
    {
        return Some(Arc::clone(tag));
    }
    candidates
        .map(|tag| (strsim::jaro_winkler(ref_name, tag.version().as_str()), tag))
        .filter(|(score, _)| *score >= MIN_SIMILARITY)
        .max_by(|(a, x), (b, y)| a.total_cmp(b).then_with(|| x.version().cmp(y.version())))
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
        closest_tag(ref_name, &set(tags)).map(|t| t.version().to_string())
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
        assert_eq!(suggest("v4.2-rc", &tags).as_deref(), Some("v4.2.4-rc.1"));
    }

    #[test]
    fn typos_name_the_most_similar_tag() {
        let tags = ["v3.9.0", "v4.1.0", "v4.2.0"];
        assert_eq!(suggest("v42.0", &tags).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("v4.2.0.", &tags).as_deref(), Some("v4.2.0"));
        assert_eq!(suggest("vv4.1.0", &tags).as_deref(), Some("v4.1.0"));
    }

    #[test]
    fn a_tie_goes_to_the_higher_version() {
        assert_eq!(
            suggest("v1.1.0", &["v1.0.0", "v1.2.0"]).as_deref(),
            Some("v1.2.0")
        );
    }

    #[test]
    fn unrelated_refs_and_prereleases_get_no_suggestion() {
        let tags = ["v4.1.0", "v4.2.0", "v5.0.0-rc.1"];
        assert_eq!(suggest("v9.9.9", &tags), None);
        assert_eq!(suggest("main", &tags), None);
        assert_eq!(suggest("v5", &tags), None);
        assert_eq!(suggest("v4.2.0", &["v4.2.0"]), None);
        assert_eq!(suggest("v4.2", &[]), None);
    }
}
