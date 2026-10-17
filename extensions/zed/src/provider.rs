//! GitHub version fetcher using Zed's host-proxied HTTP client.
//!
//! Builds an [`actup_core::version::Set`] by querying the GitHub REST API
//! through [`zed_extension_api::http_client`]. Synchronous — Zed's `fetch()`
//! blocks in the WASM extension host.

use std::{collections::HashMap, sync::Arc};

use actup_core::version;
use zed_extension_api::http_client::{HttpMethod, HttpRequest, RedirectPolicy};

/// Tag or branch entry from the GitHub REST API (`/tags` or `/branches`).
struct GitRef {
    name: String,
    sha: String,
}

/// Fetch tags, and branches when `with_branches` is set, and build a [`version::Set`].
///
/// Uses `per_page=100` (GitHub max) and follows `Link: <...>; rel="next"`
/// pagination up to `max_pages` pages per listing.
///
/// # Errors
///
/// Returns `Err(String)` on HTTP or JSON parse failures.
pub fn fetch_version_set(
    owner: &str,
    repo: &str,
    token: Option<&str>,
    api_base: &str,
    max_pages: u8,
    with_branches: bool,
) -> Result<version::Set, String> {
    let base = format!("{api_base}/repos/{owner}/{repo}");
    let tags = fetch_all(&format!("{base}/tags?per_page=100"), token, max_pages)?;
    let mut set = build_version_set(&tags);
    if with_branches {
        let branches = fetch_all(&format!("{base}/branches?per_page=100"), token, max_pages)?;
        set.set_branches(branch_map(branches));
    }
    Ok(set)
}

fn branch_map(branches: Vec<GitRef>) -> HashMap<String, String> {
    branches.into_iter().map(|b| (b.name, b.sha)).collect()
}

/// Fetch every page of a tag or branch listing.
fn fetch_all(first_url: &str, token: Option<&str>, max_pages: u8) -> Result<Vec<GitRef>, String> {
    let mut all_tags = Vec::new();
    let mut url = first_url.to_string();
    let mut page = 0u8;

    loop {
        if page >= max_pages {
            break;
        }
        page += 1;

        let mut builder = HttpRequest::builder()
            .method(HttpMethod::Get)
            .url(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "actup-zed-extension")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .redirect_policy(RedirectPolicy::FollowAll);

        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {t}"));
        }

        let request = builder.build()?;
        let response = request.fetch()?;

        let body = String::from_utf8(response.body)
            .map_err(|e| format!("invalid UTF-8 in response: {e}"))?;

        let tags = parse_refs_json(&body)?;
        all_tags.extend(tags);

        match extract_next_url(&response.headers) {
            Some(next) => url = next,
            None => break,
        }
    }

    Ok(all_tags)
}

/// Parse the JSON array of tag or branch objects from the GitHub REST API.
///
/// Note: Zed's `HttpResponse` does not expose the HTTP status code (see
/// `wit/since_v0.6.0/http-client.wit`), so error responses can only be detected
/// by the JSON shape. GitHub error responses are objects with a `message`
/// field rather than arrays — we surface that message directly so the user
/// gets a useful diagnostic instead of "expected JSON array".
fn parse_refs_json(json: &str) -> Result<Vec<GitRef>, String> {
    // Minimal JSON parsing — avoid pulling in a full JSON crate dep.
    // GitHub returns: [{"name":"v4.2.2","commit":{"sha":"abc..."},...},...]
    // We use serde_json via actup-core's transitive dependency.
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("JSON parse error: {e}"))?;

    let arr = value.as_array().ok_or_else(|| {
        // Likely an error response like {"message": "Not Found", ...} or
        // {"message": "API rate limit exceeded", ...}. Surface the message.
        value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || {
                    let excerpt: String = json.chars().take(120).collect();
                    format!("expected JSON array of refs, got: {excerpt}")
                },
                |msg| format!("GitHub API error: {msg}"),
            )
    })?;

    let mut tags = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "missing ref name".to_string())?;
        let sha = item
            .get("commit")
            .and_then(|c| c.get("sha"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "missing commit sha".to_string())?;
        tags.push(GitRef {
            name: name.to_string(),
            sha: sha.to_string(),
        });
    }

    Ok(tags)
}

/// Extract `Link: <url>; rel="next"` from response headers.
fn extract_next_url(headers: &[(String, String)]) -> Option<String> {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("link") {
            // Parse Link header: <url>; rel="next", <url>; rel="last"
            for part in value.split(',') {
                let part = part.trim();
                if part.contains("rel=\"next\"")
                    && let Some(start) = part.find('<')
                    && let Some(end) = part.find('>')
                {
                    return Some(part[start + 1..end].to_string());
                }
            }
        }
    }
    None
}

/// Build a [`version::Set`] from raw tag data.
fn build_version_set(tags: &[GitRef]) -> version::Set {
    let mut semver_tags: Vec<Arc<version::Info>> = Vec::new();
    let mut latest_per_major: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut major_tags: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut overall_latest: Option<Arc<version::Info>> = None;

    for tag in tags {
        let ver = version::Version::parse(&tag.name);
        if !ver.is_semver() {
            continue;
        }

        let info = Arc::new(version::Info::new(ver.clone(), &tag.sha));
        semver_tags.push(Arc::clone(&info));

        // Track major-only tags (e.g., "v4", "v5")
        if ver.is_major_only()
            && let Some(major) = ver.major()
        {
            major_tags.insert(major, Arc::clone(&info));
        }

        // Track latest per major
        if let Some(major) = ver.major() {
            let is_newer = latest_per_major
                .get(&major)
                .is_none_or(|existing| ver > *existing.version());
            if is_newer {
                latest_per_major.insert(major, Arc::clone(&info));
            }
        }

        // Track overall latest (skip prerelease for overall)
        if !ver.is_prerelease() {
            let is_newer = overall_latest
                .as_ref()
                .is_none_or(|existing| ver > *existing.version());
            if is_newer {
                overall_latest = Some(Arc::clone(&info));
            }
        }
    }

    version::Set::build(semver_tags, latest_per_major, major_tags, overall_latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_refs_json() -> Result<(), String> {
        let json = r#"[
            {"name": "v4.2.2", "commit": {"sha": "abc123"}},
            {"name": "v4.2.1", "commit": {"sha": "def456"}},
            {"name": "v4", "commit": {"sha": "abc123"}}
        ]"#;
        let tags = parse_refs_json(json)?;
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0].name, "v4.2.2");
        assert_eq!(tags[0].sha, "abc123");
        Ok(())
    }

    #[test]
    fn branch_listing_feeds_branch_heads() -> Result<(), String> {
        let json = r#"[
            {"name": "main", "commit": {"sha": "head-main", "url": "u"}, "protected": true},
            {"name": "release/v1", "commit": {"sha": "head-r1", "url": "u"}, "protected": false}
        ]"#;
        let mut set = build_version_set(&[]);
        set.set_branches(branch_map(parse_refs_json(json)?));
        assert_eq!(set.branch_sha("main"), Some("head-main"));
        assert_eq!(set.branch_sha("release/v1"), Some("head-r1"));
        assert_eq!(set.branch_sha("nope"), None);

        let main = actup_core::ActionRef::new(None, "actions", "checkout", None, "main");
        let result = actup_core::check_action(&main, &set, &actup_core::Config::default());
        assert_eq!(result.status(), &actup_core::CheckStatus::UpToDate);
        Ok(())
    }

    #[test]
    fn test_parse_refs_json_empty() -> Result<(), String> {
        let tags = parse_refs_json("[]")?;
        assert!(tags.is_empty());
        Ok(())
    }

    #[test]
    fn test_extract_next_url() {
        let headers = vec![(
            "Link".to_string(),
            r#"<https://api.github.com/repos/o/r/tags?page=2>; rel="next", <https://api.github.com/repos/o/r/tags?page=5>; rel="last""#.to_string(),
        )];
        let next = extract_next_url(&headers);
        assert_eq!(
            next.as_deref(),
            Some("https://api.github.com/repos/o/r/tags?page=2")
        );
    }

    #[test]
    fn test_extract_next_url_none() {
        let headers = vec![(
            "Link".to_string(),
            r#"<https://api.github.com/repos/o/r/tags?page=5>; rel="last""#.to_string(),
        )];
        assert!(extract_next_url(&headers).is_none());
    }

    #[test]
    fn test_build_version_set() -> Result<(), &'static str> {
        let tags = vec![
            GitRef {
                name: "v4.2.2".to_string(),
                sha: "sha422".to_string(),
            },
            GitRef {
                name: "v4.1.0".to_string(),
                sha: "sha410".to_string(),
            },
            GitRef {
                name: "v4".to_string(),
                sha: "sha4".to_string(),
            },
            GitRef {
                name: "v3.5.0".to_string(),
                sha: "sha350".to_string(),
            },
            GitRef {
                name: "not-semver".to_string(),
                sha: "xyz".to_string(),
            },
        ];
        let set = build_version_set(&tags);

        // 4 semver tags (not-semver excluded)
        assert_eq!(set.semver_tags().len(), 4);

        // latest overall should be v4.2.2
        let latest = set.latest().ok_or("expected latest")?;
        assert_eq!(latest.version().to_string(), "v4.2.2");

        // latest per major: v3 -> v3.5.0, v4 -> v4.2.2
        let v3 = set.latest_per_major().get(&3).ok_or("expected major 3")?;
        assert_eq!(v3.version().to_string(), "v3.5.0");

        let v4 = set.latest_per_major().get(&4).ok_or("expected major 4")?;
        assert_eq!(v4.version().to_string(), "v4.2.2");

        // major tag: v4 exists
        assert!(set.major_tags().contains_key(&4));
        let major4 = set.major_tags().get(&4).ok_or("expected major tag 4")?;
        assert_eq!(major4.version().to_string(), "v4");
        Ok(())
    }
}
