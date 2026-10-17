//! GitHub version fetcher using Zed's host-proxied HTTP client.
//!
//! Builds an [`actup_core::version::Set`] by querying the GitHub REST API
//! through [`zed_extension_api::http_client`]. Synchronous — Zed's `fetch()`
//! blocks in the WASM extension host.

use std::{
    collections::HashMap,
    future::Future,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use actup_core::{
    ActionRef, ActionRepoId, BoxFuture, CheckResult, CheckStatus, Config, Error,
    check_action_with_provider,
    provider::{ActionYml, Kind, Provider, Sha40},
    version,
};
use zed_extension_api::http_client::{HttpMethod, HttpRequest, RedirectPolicy};

/// Body and headers of an HTTP response.
#[derive(Clone)]
pub struct Response {
    body: String,
    headers: Vec<(String, String)>,
}

/// Synchronous HTTP GET against a GitHub-compatible REST API.
pub trait Fetch {
    /// Fetch `url`.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` when the request fails.
    fn get(&self, url: &str) -> Result<Response, String>;
}

/// [`Fetch`] through Zed's host-proxied HTTP client.
pub struct ZedHttp {
    token: Option<String>,
}

impl ZedHttp {
    /// Create a client that authenticates with `token` when set.
    pub const fn new(token: Option<String>) -> Self {
        Self { token }
    }
}

impl Fetch for ZedHttp {
    fn get(&self, url: &str) -> Result<Response, String> {
        let mut builder = HttpRequest::builder()
            .method(HttpMethod::Get)
            .url(url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "actup-zed-extension")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .redirect_policy(RedirectPolicy::FollowAll);

        if let Some(t) = &self.token {
            builder = builder.header("Authorization", format!("Bearer {t}"));
        }

        let response = builder.build()?.fetch()?;
        let body = String::from_utf8(response.body)
            .map_err(|e| format!("invalid UTF-8 in response: {e}"))?;
        Ok(Response {
            body,
            headers: response.headers,
        })
    }
}

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
/// Returns `Err(String)` on HTTP or JSON parse failures of the tag listing. A
/// failed branch listing keeps the tags.
pub fn fetch_version_set(
    fetch: &impl Fetch,
    owner: &str,
    repo: &str,
    api_base: &str,
    max_pages: u8,
    with_branches: bool,
) -> Result<version::Set, String> {
    let base = format!("{api_base}/repos/{owner}/{repo}");
    let tags = fetch_all(fetch, &format!("{base}/tags?per_page=100"), max_pages)?;
    let mut set = build_version_set(&tags);
    if with_branches
        && let Ok(branches) = fetch_all(fetch, &format!("{base}/branches?per_page=100"), max_pages)
    {
        set.set_branches(branch_map(branches));
    }
    Ok(set)
}

fn branch_map(branches: Vec<GitRef>) -> HashMap<String, String> {
    branches.into_iter().map(|b| (b.name, b.sha)).collect()
}

/// Fetch every page of a tag or branch listing.
fn fetch_all(fetch: &impl Fetch, first_url: &str, max_pages: u8) -> Result<Vec<GitRef>, String> {
    let mut all_tags = Vec::new();
    let mut url = first_url.to_string();
    let mut page = 0u8;

    loop {
        if page >= max_pages {
            break;
        }
        page += 1;

        let response = fetch.get(&url)?;
        let tags = parse_refs_json(&response.body)?;
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
    let mut other_tags: HashMap<String, String> = HashMap::new();

    for tag in tags {
        let ver = version::Version::parse(&tag.name);
        if !ver.is_semver() {
            other_tags.insert(tag.name.clone(), tag.sha.clone());
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

    let mut set = version::Set::build(semver_tags, latest_per_major, major_tags, overall_latest);
    set.set_other_tags(other_tags);
    set
}

/// [`Provider`] that resolves refs through a GitHub-compatible REST API.
pub struct RestProvider<F> {
    fetch: F,
    api_base: String,
    host: String,
}

impl<F: Fetch> RestProvider<F> {
    /// Create a provider for `host` whose REST API lives at `api_base`.
    pub fn new(fetch: F, api_base: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            fetch,
            api_base: api_base.into(),
            host: host.into(),
        }
    }

    fn resolve(&self, repo: &ActionRepoId, ref_name: &str) -> actup_core::Result<Sha40> {
        let url = format!(
            "{}/repos/{}/{}/commits/{ref_name}",
            self.api_base,
            repo.owner(),
            repo.repo()
        );
        let response = self.fetch.get(&url).map_err(|message| Error::Network {
            operation: format!("GET {url}"),
            message,
        })?;
        let value: serde_json::Value =
            serde_json::from_str(&response.body).map_err(|e| Error::Network {
                operation: format!("parse {url}"),
                message: e.to_string(),
            })?;
        if let Some(sha) = value
            .get("sha")
            .and_then(serde_json::Value::as_str)
            .and_then(Sha40::new)
        {
            return Ok(sha);
        }
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Err(api_error(message, repo, ref_name, &self.host))
    }
}

// Zed's `HttpResponse` carries no status code.
fn api_error(message: &str, repo: &ActionRepoId, ref_name: &str, host: &str) -> Error {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit") {
        Error::RateLimited {
            provider: "github".into(),
            retry_after: None,
        }
    } else if lower.contains("bad credentials") {
        Error::Auth {
            provider: "github".into(),
            reason: message.to_string(),
        }
    } else if lower.contains("not found") || lower.contains("no commit found") {
        Error::RefNotFound {
            owner: repo.owner().to_string(),
            repo: repo.repo().to_string(),
            ref_name: ref_name.to_string(),
            host: Some(host.to_string()),
        }
    } else {
        Error::Network {
            operation: "resolve ref".into(),
            message: format!("unexpected GitHub response: {message}"),
        }
    }
}

fn unsupported<T>(operation: &str) -> actup_core::Result<T> {
    Err(Error::Internal {
        context: operation.to_string(),
        message: "not supported in the Zed extension".to_string(),
    })
}

impl<F: Fetch + Send + Sync> Provider for RestProvider<F> {
    fn kind(&self) -> Kind {
        Kind::GitHub
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<version::Set>> {
        let result = fetch_version_set(
            &self.fetch,
            repo.owner(),
            repo.repo(),
            &self.api_base,
            MAX_PAGES,
            true,
        )
        .map_err(|message| Error::Network {
            operation: "list refs".into(),
            message,
        });
        Box::pin(async move { result })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
        let result = self.resolve(repo, ref_name);
        Box::pin(async move { result })
    }

    fn get_default_branch(
        &self,
        _repo: &ActionRepoId,
    ) -> BoxFuture<'_, actup_core::Result<String>> {
        Box::pin(async { unsupported("default branch") })
    }

    fn get_readme(
        &self,
        _repo: &ActionRepoId,
        _ref_name: Option<&str>,
    ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
        Box::pin(async { unsupported("readme") })
    }

    fn get_action_yml(
        &self,
        _repo: &ActionRepoId,
        _ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
        Box::pin(async { unsupported("action.yml") })
    }
}

/// Pages of 100 refs fetched per tag or branch listing.
pub const MAX_PAGES: u8 = 3;

/// Check `action_ref` against `versions`, asking `provider` about refs the listing lacks.
pub fn check(
    action_ref: &ActionRef,
    versions: &version::Set,
    config: &Config,
    provider: &dyn Provider,
) -> CheckResult {
    ready(check_action_with_provider(
        action_ref, versions, config, provider,
    ))
    .unwrap_or_else(|| {
        CheckResult::new(
            action_ref.clone(),
            CheckStatus::Unresolvable {
                reason: "ref lookup did not complete".into(),
                is_environment_error: true,
            },
        )
    })
}

fn ready<T>(future: impl Future<Output = T>) -> Option<T> {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(future).poll(&mut context) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use actup_core::policy;

    use super::*;

    const API: &str = "https://api.test";
    const FULL_SHA: &str = "abc1234def5678abc1234def5678abc1234def56";

    #[derive(Default)]
    struct MockFetch {
        routes: HashMap<String, Result<Response, String>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockFetch {
        fn route(mut self, url: &str, body: &str) -> Self {
            self.routes.insert(
                url.to_string(),
                Ok(Response {
                    body: body.to_string(),
                    headers: Vec::new(),
                }),
            );
            self
        }

        fn paged(mut self, url: &str, body: &str, next: &str) -> Self {
            let headers = vec![("Link".to_string(), format!("<{next}>; rel=\"next\""))];
            self.routes.insert(
                url.to_string(),
                Ok(Response {
                    body: body.to_string(),
                    headers,
                }),
            );
            self
        }

        fn failing(mut self, url: &str) -> Self {
            self.routes
                .insert(url.to_string(), Err("connection reset".to_string()));
            self
        }

        fn requested(&self) -> Vec<String> {
            self.requests.lock().map(|r| r.clone()).unwrap_or_default()
        }

        fn log(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.requests)
        }
    }

    impl Fetch for MockFetch {
        fn get(&self, url: &str) -> Result<Response, String> {
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(url.to_string());
            }
            self.routes
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("unexpected request: {url}")))
        }
    }

    const TAGS_URL: &str = "https://api.test/repos/o/r/tags?per_page=100";
    const BRANCHES_URL: &str = "https://api.test/repos/o/r/branches?per_page=100";
    const TAGS: &str = r#"[{"name": "v1.0.0", "commit": {"sha": "t1"}}, {"name": "stable", "commit": {"sha": "t2"}}]"#;

    #[test]
    fn fetch_skips_branches_unless_asked() -> Result<(), String> {
        let fetch = MockFetch::default().route(TAGS_URL, TAGS);
        let set = fetch_version_set(&fetch, "o", "r", API, MAX_PAGES, false)?;
        assert_eq!(fetch.requested(), [TAGS_URL]);
        assert_eq!(set.semver_tags().len(), 1);
        assert_eq!(set.other_tag_sha("stable"), Some("t2"));
        Ok(())
    }

    #[test]
    fn fetch_follows_branch_pagination() -> Result<(), String> {
        let page2 = "https://api.test/repos/o/r/branches?per_page=100&page=2";
        let fetch = MockFetch::default()
            .route(TAGS_URL, TAGS)
            .paged(
                BRANCHES_URL,
                r#"[{"name": "main", "commit": {"sha": "b1"}}]"#,
                page2,
            )
            .route(page2, r#"[{"name": "dev", "commit": {"sha": "b2"}}]"#);
        let set = fetch_version_set(&fetch, "o", "r", API, MAX_PAGES, true)?;
        assert_eq!(set.branch_sha("main"), Some("b1"));
        assert_eq!(set.branch_sha("dev"), Some("b2"));
        assert_eq!(fetch.requested(), [TAGS_URL, BRANCHES_URL, page2]);
        Ok(())
    }

    #[test]
    fn branch_failure_keeps_tags() -> Result<(), String> {
        let fetch = MockFetch::default()
            .route(TAGS_URL, TAGS)
            .failing(BRANCHES_URL);
        let set = fetch_version_set(&fetch, "o", "r", API, MAX_PAGES, true)?;
        assert_eq!(set.semver_tags().len(), 1);
        assert_eq!(set.branch_sha("main"), None);
        Ok(())
    }

    #[test]
    fn tag_failure_is_an_error() {
        let fetch = MockFetch::default().failing(TAGS_URL);
        assert_eq!(
            fetch_version_set(&fetch, "o", "r", API, MAX_PAGES, true).err(),
            Some("connection reset".to_string())
        );
    }

    fn rest(fetch: MockFetch) -> RestProvider<MockFetch> {
        RestProvider::new(fetch, API, "github.com")
    }

    fn lookup(body: &str) -> actup_core::Result<Sha40> {
        let provider =
            rest(MockFetch::default().route("https://api.test/repos/o/r/commits/x", body));
        provider.resolve(&ActionRepoId::simple("o", "r"), "x")
    }

    #[test]
    fn resolve_reads_the_commit_sha() {
        let found = lookup(&format!(r#"{{"sha": "{FULL_SHA}"}}"#));
        assert_eq!(
            found.ok().map(|s| s.as_str().to_string()).as_deref(),
            Some(FULL_SHA)
        );
    }

    #[test]
    fn resolve_classifies_error_messages() {
        assert!(matches!(
            lookup(r#"{"message": "No commit found for SHA: x"}"#),
            Err(Error::RefNotFound { .. })
        ));
        assert!(matches!(
            lookup(r#"{"message": "Not Found"}"#),
            Err(Error::RefNotFound { .. })
        ));
        assert!(matches!(
            lookup(r#"{"message": "API rate limit exceeded for 1.2.3.4."}"#),
            Err(Error::RateLimited { .. })
        ));
        assert!(matches!(
            lookup(r#"{"message": "Bad credentials"}"#),
            Err(Error::Auth { .. })
        ));
        assert!(matches!(lookup("<html>"), Err(Error::Network { .. })));
    }

    fn check_ref(ref_str: &str, body: &str, branch: policy::Branch) -> CheckStatus {
        let url = format!("https://api.test/repos/actions/checkout/commits/{ref_str}");
        let provider = rest(MockFetch::default().route(&url, body));
        let config = Config {
            policy: actup_core::config::Policy {
                branch,
                ..Default::default()
            },
            ..Default::default()
        };
        let action = ActionRef::new(None, "actions", "checkout", None, ref_str);
        check(&action, &build_version_set(&[]), &config, &provider)
            .status()
            .clone()
    }

    #[test]
    fn check_through_caching_provider_looks_up_each_ref_once() {
        let url = "https://api.test/repos/actions/checkout/commits/nope";
        let fetch = MockFetch::default().route(url, r#"{"message": "Not Found"}"#);
        let log = fetch.log();
        let cached = actup_core::CachingProvider::new(rest(fetch));
        let action = ActionRef::new(None, "actions", "checkout", None, "nope");
        for _ in 0..2 {
            let result = check(
                &action,
                &build_version_set(&[]),
                &Config::default(),
                &cached,
            );
            assert_eq!(
                result.status(),
                &CheckStatus::Unresolvable {
                    reason: "ref 'nope' not found".to_string(),
                    is_environment_error: false,
                }
            );
        }
        assert_eq!(log.lock().map(|r| r.clone()).unwrap_or_default(), [url]);
    }

    #[test]
    fn provider_lists_tags_and_branches_and_rejects_other_calls() {
        let provider = rest(MockFetch::default().route(TAGS_URL, TAGS).route(
            BRANCHES_URL,
            r#"[{"name": "main", "commit": {"sha": "b1"}}]"#,
        ));
        let repo = ActionRepoId::simple("o", "r");
        let Some(Ok(versions)) = ready(provider.get_versions(&repo)) else {
            panic!("expected get_versions to complete");
        };
        assert_eq!(versions.branch_sha("main"), Some("b1"));
        assert_eq!(versions.other_tag_sha("stable"), Some("t2"));
        assert!(matches!(
            ready(provider.get_default_branch(&repo)),
            Some(Err(Error::Internal { .. }))
        ));
        assert!(matches!(
            ready(provider.get_readme(&repo, None)),
            Some(Err(Error::Internal { .. }))
        ));
        assert!(matches!(
            ready(provider.get_action_yml(&repo, "v1")),
            Some(Err(Error::Internal { .. }))
        ));
        assert_eq!(provider.host(), "github.com");
        assert_eq!(provider.kind(), Kind::GitHub);
    }

    #[test]
    fn ready_reports_a_pending_future() {
        assert_eq!(ready(std::future::pending::<()>()), None);
        assert_eq!(ready(async { 1 }), Some(1));
    }

    #[test]
    fn check_looks_up_refs_the_listing_lacks() {
        let found = format!(r#"{{"sha": "{FULL_SHA}"}}"#);
        assert_eq!(
            check_ref("stable", &found, policy::Branch::Warn),
            CheckStatus::Floating {
                ref_name: "stable".to_string()
            }
        );
        assert_eq!(
            check_ref("nope", r#"{"message": "Not Found"}"#, policy::Branch::Warn),
            CheckStatus::Unresolvable {
                reason: "ref 'nope' not found".to_string(),
                is_environment_error: false,
            }
        );
        assert_eq!(
            check_ref("abc1234", &found, policy::Branch::Ignore),
            CheckStatus::Unresolvable {
                reason: actup_core::SHORT_SHA_REASON.to_string(),
                is_environment_error: false,
            }
        );
    }

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
