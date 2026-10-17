//! Integration tests for GitHub provider with mocked HTTP responses

use std::num::NonZeroU8;

use actup_core::{ActionRepoId, Provider};
use actup_provider_github::{GitHubClient, GitHubProvider};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TAGS_RESPONSE: &str = include_str!("fixtures/tags_response.json");
const REPO_RESPONSE: &str = include_str!("fixtures/repo_response.json");
const BRANCH_RESPONSE: &str = include_str!("fixtures/branch_response.json");

#[tokio::test]
async fn test_list_tags() -> TestResult {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TAGS_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let tags = client.list_tags("actions", "checkout", None).await?;

    assert_eq!(tags.len(), 5);
    assert_eq!(tags[0].name, "v6.0.2");
    assert_eq!(
        tags[0].commit.sha,
        "de0fac2e4500dabe0009e67214ff5f5447ce83dd"
    );
    Ok(())
}

#[tokio::test]
async fn test_list_tags_limited_stops_after_requested_count() -> TestResult {
    let mock_server = MockServer::start().await;
    let next_url = format!(
        "{}/repos/actions/checkout/tags?per_page=2&page=2",
        mock_server.uri()
    );

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/tags"))
        .and(query_param("per_page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", format!("<{next_url}>; rel=\"next\""))
                .set_body_string(TAGS_RESPONSE),
        )
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let tags = client
        .list_tags_limited("actions", "checkout", NonZeroU8::MIN.saturating_add(1))
        .await?;

    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].name, "v6.0.2");
    assert_eq!(tags[1].name, "v6.0.1");
    Ok(())
}

#[tokio::test]
async fn test_get_repo() -> TestResult {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout"))
        .respond_with(ResponseTemplate::new(200).set_body_string(REPO_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let repo = client.get_repo("actions", "checkout").await?;

    assert_eq!(repo.default_branch, "main");
    Ok(())
}

#[tokio::test]
async fn test_get_branch() -> TestResult {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BRANCH_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let branch = client.get_branch("actions", "checkout", "main").await?;

    assert_eq!(
        branch.commit.sha,
        "0c366fd6a839edf440554fa01a7085ccba70ac98"
    );
    Ok(())
}

#[tokio::test]
async fn test_not_found_error() -> TestResult {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/nonexistent/repo"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let result = client.get_repo("nonexistent", "repo").await;

    assert!(result.is_err());
    let err = result.err().ok_or("`result` did not fail")?;
    assert!(matches!(err, actup_core::Error::NotFound { .. }));
    Ok(())
}

#[tokio::test]
async fn test_rate_limit_error() -> TestResult {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("retry-after", "60")
                .set_body_string(r#"{"message": "API rate limit exceeded"}"#),
        )
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)?;

    let result = client.get_repo("actions", "checkout").await;

    assert!(result.is_err());
    let err = result.err().ok_or("`result` did not fail")?;
    assert!(matches!(err, actup_core::Error::RateLimited { .. }));
    Ok(())
}

async fn serve_rest_versions(server: &MockServer, per_page: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/tags"))
        .and(query_param("per_page", per_page))
        .respond_with(ResponseTemplate::new(200).set_body_string(TAGS_RESPONSE))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout"))
        .respond_with(ResponseTemplate::new(200).set_body_string(REPO_RESPONSE))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BRANCH_RESPONSE))
        .mount(server)
        .await;
}

#[tokio::test]
async fn rest_versions_fetch_the_core_default_tag_count() -> Result<(), Box<dyn std::error::Error>>
{
    let server = MockServer::start().await;
    serve_rest_versions(
        &server,
        &actup_core::config::DEFAULT_MAX_TAGS_PER_REPO.to_string(),
    )
    .await;

    let set = provider(&server)?
        .versions_via_rest(&ActionRepoId::simple("actions", "checkout"))
        .await?;

    assert_eq!(set.semver_tags().len(), 5);
    Ok(())
}

#[tokio::test]
async fn rest_versions_fetch_at_most_max_tags() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    serve_rest_versions(&server, "1").await;

    let set = provider(&server)?
        .max_tags(NonZeroU8::MIN)
        .versions_via_rest(&ActionRepoId::simple("actions", "checkout"))
        .await?;

    let names: Vec<&str> = set
        .semver_tags()
        .iter()
        .map(|t| t.version().as_str())
        .collect();
    assert_eq!(names, ["v6.0.2"]);
    Ok(())
}

const FULL_SHA: &str = "abc1234def5678abc1234def5678abc1234def56";

async fn mount_not_found(server: &MockServer, url: &str) {
    Mock::given(method("GET"))
        .and(path(url.to_string()))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#))
        .mount(server)
        .await;
}

fn provider(server: &MockServer) -> Result<GitHubProvider, Box<dyn std::error::Error>> {
    Ok(GitHubProvider::with_host(
        &server.uri(),
        "github.com",
        None,
    )?)
}

#[tokio::test]
async fn resolve_short_sha_falls_back_to_commit() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    mount_not_found(&server, "/repos/o/r/git/ref/tags/abc1234").await;
    mount_not_found(&server, "/repos/o/r/branches/abc1234").await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/abc1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!(r#"{{"sha": "{FULL_SHA}"}}"#)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&ActionRepoId::simple("o", "r"), "abc1234")
        .await?;
    assert_eq!(sha.as_str(), FULL_SHA);
    Ok(())
}

#[tokio::test]
async fn resolve_hex_branch_name_wins_over_commit() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    mount_not_found(&server, "/repos/o/r/git/ref/tags/deadbee").await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/branches/deadbee"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"name": "deadbee", "commit": {{"sha": "{FULL_SHA}"}}}}"#
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/deadbee"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&ActionRepoId::simple("o", "r"), "deadbee")
        .await?;
    assert_eq!(sha.as_str(), FULL_SHA);
    Ok(())
}

#[tokio::test]
async fn resolve_missing_name_skips_commit_lookup() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    mount_not_found(&server, "/repos/o/r/git/ref/tags/nope").await;
    mount_not_found(&server, "/repos/o/r/branches/nope").await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/nope"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let result = provider(&server)?
        .resolve_ref_to_sha(&ActionRepoId::simple("o", "r"), "nope")
        .await;
    assert!(
        matches!(result, Err(actup_core::Error::NotFound { .. })),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn resolve_unknown_short_sha_is_ref_not_found() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    mount_not_found(&server, "/repos/o/r/git/ref/tags/abc1234").await;
    mount_not_found(&server, "/repos/o/r/branches/abc1234").await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/abc1234"))
        .respond_with(
            ResponseTemplate::new(422).set_body_string(
                r#"{"message": "No commit found for SHA: abc1234", "status": "422"}"#,
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let result = provider(&server)?
        .resolve_ref_to_sha(&ActionRepoId::simple("o", "r"), "abc1234")
        .await;
    let Err(actup_core::Error::RefNotFound { ref_name, .. }) = result else {
        panic!("expected RefNotFound, got {result:?}");
    };
    assert_eq!(ref_name, "abc1234");
    Ok(())
}

#[tokio::test]
async fn batch_fetch_reports_a_failed_repo_after_one_attempt()
-> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use actup_core::{
        Config, Retention, cache,
        config::{ApiMode, Host, HostKind},
        provider::Registry,
    };

    let server = MockServer::start().await;
    // RFC 2606 reserves `.invalid`, so no server answers the git listing.
    let host = "ghe.invalid".to_string();
    Mock::given(method("GET"))
        .and(path("/repos/o/gone/tags"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/limited/tags"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_string(r#"{"message": "API rate limit exceeded"}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = Config {
        default_host: Some(host.clone()),
        ..Config::default()
    };
    config.hosts.insert(
        host.clone(),
        Host {
            kind: Some(HostKind::GitHub),
            api_base: Some(server.uri()),
            token_env: Some("ACTUP_TEST_TOKEN_THAT_IS_NEVER_SET".to_string()),
            api_mode: Some(ApiMode::Rest),
            max_tags_per_repo: None,
        },
    );
    let registry = Registry::from_config(
        &config,
        &[&actup_provider_github::REGISTRATION],
        &Retention::Run,
    )?;
    let gone = ActionRepoId::new(Some(host.clone()), "o", "gone", None);
    let limited = ActionRepoId::new(Some(host.clone()), "o", "limited", None);

    let results = actup_provider_github::fetch_versions_with_registry(
        &[gone.clone(), limited.clone()],
        &config,
        &registry,
        Arc::new(cache::Noop),
        &actup_core::fetch::Silent,
    )
    .await;

    assert!(
        matches!(
            results.get(&gone),
            Some(Err(actup_core::Error::NotFound { .. }))
        ),
        "{:?}",
        results.get(&gone)
    );
    assert!(
        matches!(
            results.get(&limited),
            Some(Err(actup_core::Error::RateLimited { .. }))
        ),
        "{:?}",
        results.get(&limited)
    );
    let requests = server.received_requests().await.unwrap_or_default();
    let paths: Vec<&str> = requests.iter().map(|request| request.url.path()).collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    Ok(())
}

#[tokio::test]
async fn batch_fetch_asks_rest_about_repos_graphql_leaves_out()
-> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use actup_core::cache;
    use actup_provider_github::BatchFetcher;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data": {"repo0": null}}"#))
        .expect(1)
        .mount(&server)
        .await;
    mount_not_found(&server, "/repos/o/gone/tags").await;
    let provider = GitHubProvider::with_host(&server.uri(), "ghe.invalid", Some("token"))?;
    let fetcher = BatchFetcher::with_graphql(
        provider,
        Arc::new(cache::Noop),
        Some("token"),
        NonZeroU8::MIN.saturating_add(9),
    );
    let gone = ActionRepoId::new(Some("ghe.invalid".to_string()), "o", "gone", None);

    let results = fetcher
        .fetch_versions(std::slice::from_ref(&gone), &actup_core::fetch::Silent)
        .await;

    assert!(fetcher.has_graphql());
    assert!(
        matches!(
            results.get(&gone),
            Some(Err(actup_core::Error::NotFound { .. }))
        ),
        "{:?}",
        results.get(&gone)
    );
    let requests = server.received_requests().await.unwrap_or_default();
    let paths: Vec<&str> = requests.iter().map(|request| request.url.path()).collect();
    assert_eq!(paths, ["/api/graphql", "/repos/o/gone/tags"]);
    Ok(())
}

struct Recorder {
    events: std::sync::Mutex<Vec<String>>,
}

impl actup_core::fetch::Progress for Recorder {
    fn start(&self, repos: usize) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("start {repos}"));
    }

    fn finished(&self, outcome: actup_core::fetch::Outcome) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{outcome:?}"));
    }
}

struct Hits(Vec<String>);

impl actup_core::cache::Cache for Hits {
    fn get(
        &self,
        key: &str,
    ) -> actup_core::BoxFuture<'_, actup_core::cache::Result<actup_core::cache::Entry>> {
        let hit = self.0.iter().any(|k| k == key);
        Box::pin(async move {
            if hit {
                Ok(actup_core::cache::Entry::new(
                    actup_core::version::Set::new(),
                ))
            } else {
                Err(actup_core::cache::Error::NotFound)
            }
        })
    }

    fn set(
        &self,
        _key: &str,
        _entry: actup_core::cache::Entry,
    ) -> actup_core::BoxFuture<'_, actup_core::cache::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, _key: &str) -> actup_core::BoxFuture<'_, actup_core::cache::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn clear(&self) -> actup_core::BoxFuture<'_, actup_core::cache::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn unused<T: Send + 'static>() -> actup_core::BoxFuture<'static, actup_core::Result<T>> {
    Box::pin(async {
        Err(actup_core::Error::Internal {
            context: "test provider".to_string(),
            message: "not used".to_string(),
            source: None,
        })
    })
}

struct Listed;

impl Provider for Listed {
    fn kind(&self) -> actup_core::provider::Kind {
        actup_core::provider::Kind::GitLab
    }

    fn host(&self) -> &'static str {
        "lab.example"
    }

    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::version::Set>> {
        let result = if repo.repo() == "ok" {
            Ok(actup_core::version::Set::new())
        } else {
            Err(actup_core::Error::NotFound {
                owner: repo.owner().to_string(),
                repo: repo.repo().to_string(),
                host: None,
            })
        };
        Box::pin(async move { result })
    }

    fn resolve_ref_to_sha(
        &self,
        _repo: &ActionRepoId,
        _ref_name: &str,
    ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::provider::Sha40>> {
        unused()
    }

    fn get_default_branch(
        &self,
        _repo: &ActionRepoId,
    ) -> actup_core::BoxFuture<'_, actup_core::Result<String>> {
        unused()
    }

    fn get_readme(
        &self,
        _repo: &ActionRepoId,
        _ref_name: Option<&str>,
    ) -> actup_core::BoxFuture<'_, actup_core::Result<Option<String>>> {
        unused()
    }

    fn get_action_yml(
        &self,
        _repo: &ActionRepoId,
        _ref_name: &str,
    ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::provider::ActionYml>> {
        unused()
    }
}

#[tokio::test]
async fn test_registry_fetch_reports_each_repo_once() -> Result<(), Box<dyn std::error::Error>> {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TAGS_RESPONSE))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_string(REPO_RESPONSE))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BRANCH_RESPONSE))
        .mount(&mock_server)
        .await;
    let mut config = actup_core::Config::default();
    config.hosts.insert(
        "gh.invalid".to_string(),
        actup_core::config::Host {
            kind: Some(actup_core::config::HostKind::GitHub),
            api_base: Some(mock_server.uri()),
            token_env: Some("_ACTUP_TEST_NO_TOKEN".to_string()),
            api_mode: Some(actup_core::config::ApiMode::Rest),
            max_tags_per_repo: None,
        },
    );
    let mut registry = actup_core::provider::Registry::new();
    registry.set_default_host("lab.example");
    registry.register(Box::new(Listed));
    registry.register(Box::new(GitHubProvider::with_host(
        &mock_server.uri(),
        "gh.invalid",
        None,
    )?));
    let repos = [
        ActionRepoId::new(Some("lab.example".into()), "o", "ok", None),
        ActionRepoId::new(Some("lab.example".into()), "o", "bad", None),
        ActionRepoId::new(Some("gh.invalid".into()), "o", "r", None),
        ActionRepoId::new(Some("none.invalid".into()), "o", "x", None),
        ActionRepoId::new(Some("lab.example".into()), "o", "cached", None),
        ActionRepoId::new(Some("gh.invalid".into()), "o", "hit", None),
    ];
    let cache = Hits(vec![
        repos[4].cache_key("gitlab", "lab.example"),
        repos[5].cache_key("github", "gh.invalid"),
    ]);
    let recorder = Recorder {
        events: std::sync::Mutex::new(Vec::new()),
    };

    let results = actup_provider_github::fetch_versions_with_registry(
        &repos,
        &config,
        &registry,
        std::sync::Arc::new(cache),
        &recorder,
    )
    .await;

    assert_eq!(results.len(), 6);
    let mut events = recorder.events.into_inner().map_err(|e| e.to_string())?;
    assert_eq!(events.remove(0), "start 6");
    events.sort_unstable();
    assert_eq!(
        events,
        ["Api", "Cache", "Cache", "Failed", "Failed", "Provider"]
    );
    Ok(())
}

#[tokio::test]
async fn batch_fetch_asks_rest_once_when_graphql_fails() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use actup_core::cache;
    use actup_provider_github::BatchFetcher;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;
    for repo in ["one", "two"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/{repo}/tags")))
            .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#))
            .expect(1)
            .mount(&server)
            .await;
    }
    let provider = GitHubProvider::with_host(&server.uri(), "ghe.invalid", Some("token"))?;
    let fetcher = BatchFetcher::with_graphql(
        provider,
        Arc::new(cache::Noop),
        Some("token"),
        NonZeroU8::MIN.saturating_add(9),
    );
    let repos: Vec<ActionRepoId> = ["one", "two"]
        .into_iter()
        .map(|repo| ActionRepoId::new(Some("ghe.invalid".to_string()), "o", repo, None))
        .collect();

    let results = fetcher
        .fetch_versions(&repos, &actup_core::fetch::Silent)
        .await;

    for repo in &repos {
        assert!(
            matches!(
                results.get(repo),
                Some(Err(actup_core::Error::NotFound { .. }))
            ),
            "{:?}",
            results.get(repo)
        );
    }
    let requests = server.received_requests().await.unwrap_or_default();
    let rest = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/tags"))
        .count();
    let graphql = requests
        .iter()
        .filter(|request| request.url.path() == "/api/graphql")
        .count();
    assert_eq!((graphql, rest), (1, 2));
    Ok(())
}

#[tokio::test]
async fn search_actions_queries_the_github_actions_topic() -> Result<(), Box<dyn std::error::Error>>
{
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/repositories"))
        .and(query_param("q", "check topic:github-actions"))
        .and(query_param("per_page", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"total_count": 2, "items": [
                {"name": "checkout", "owner": {"login": "actions"}, "description": "Check out a repo"},
                {"name": "check-spelling", "owner": {"login": "check-spelling"}, "description": null}
            ]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let found = provider(&server)?.search_actions("check").await?;
    assert_eq!(
        found,
        actup_core::provider::Search::Found(vec![
            actup_core::provider::RepoSummary {
                owner: "actions".into(),
                repo: "checkout".into(),
                description: Some("Check out a repo".into()),
            },
            actup_core::provider::RepoSummary {
                owner: "check-spelling".into(),
                repo: "check-spelling".into(),
                description: None,
            },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn search_actions_reports_a_rate_limit() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/repositories"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_string(r#"{"message": "API rate limit exceeded"}"#),
        )
        .mount(&server)
        .await;

    let result = provider(&server)?.search_actions("check").await;
    assert!(
        matches!(result, Err(actup_core::Error::RateLimited { .. })),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn repo_state_reads_archived_and_disabled() -> Result<(), Box<dyn std::error::Error>> {
    use actup_core::provider::RepoState;

    let server = MockServer::start().await;
    for (repo, archived, disabled) in [
        ("active", false, false),
        ("archived", true, false),
        ("disabled", true, true),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/actions/{repo}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"full_name": "actions/{repo}", "default_branch": "main", "archived": {archived}, "disabled": {disabled}}}"#
            )))
            .mount(&server)
            .await;
    }
    mount_not_found(&server, "/repos/actions/gone").await;

    let provider = provider(&server)?;
    let state = |repo: &str| provider.get_repo_state(&ActionRepoId::simple("actions", repo));
    assert_eq!(state("active").await?, Some(RepoState::Active));
    assert_eq!(state("archived").await?, Some(RepoState::Archived));
    assert_eq!(state("disabled").await?, Some(RepoState::Disabled));
    assert!(matches!(
        state("gone").await,
        Err(actup_core::Error::NotFound { .. })
    ));
    Ok(())
}

mod stale {
    use std::{
        collections::{BTreeMap, HashMap},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use actup_core::{
        ActionRepoId, BoxFuture, Clock, Provider,
        cache::{self, Cache, Entry, Lookup, StaleReason},
        provider::{self, ActionYml, Kind, Sha40},
        version::{self, BranchInfo, Info, Version},
    };
    use actup_provider_github::{BatchFetcher, GitHubProvider};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::{BRANCH_RESPONSE, REPO_RESPONSE, TAGS_RESPONSE};

    const HOST: &str = "ghe.invalid";
    const CACHED_SHA: &str = "1111111111111111111111111111111111111111";
    const DAY_AGO: Duration = Duration::from_hours(26);

    #[derive(Debug)]
    struct Past(SystemTime);

    impl Clock for Past {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[derive(Default)]
    struct MemoryCache(Mutex<HashMap<String, Entry>>);

    impl MemoryCache {
        fn with(key: &str, entry: Entry) -> Arc<Self> {
            let cache = Self::default();
            if let Ok(mut entries) = cache.0.lock() {
                entries.insert(key.to_string(), entry);
            }
            Arc::new(cache)
        }

        fn entry(&self, key: &str) -> Option<Entry> {
            self.0.lock().ok()?.get(key).cloned()
        }
    }

    impl Cache for MemoryCache {
        fn get(&self, key: &str) -> BoxFuture<'_, cache::Result<Entry>> {
            let lookup = self.lookup(key);
            Box::pin(async move {
                match lookup.await? {
                    Lookup::Fresh(entry) => Ok(entry),
                    Lookup::Expired(_) => Err(cache::Error::NotFound),
                }
            })
        }

        fn lookup(&self, key: &str) -> BoxFuture<'_, cache::Result<Lookup>> {
            let entry = self.entry(key);
            Box::pin(async move {
                let entry = entry.ok_or(cache::Error::NotFound)?;
                Ok(if entry.is_expired(Duration::from_hours(1)) {
                    Lookup::Expired(entry)
                } else {
                    Lookup::Fresh(entry)
                })
            })
        }

        fn set(&self, key: &str, entry: Entry) -> BoxFuture<'_, cache::Result<()>> {
            if let Ok(mut entries) = self.0.lock() {
                entries.insert(key.to_string(), entry);
            }
            Box::pin(async { Ok(()) })
        }

        fn remove(&self, key: &str) -> BoxFuture<'_, cache::Result<()>> {
            if let Ok(mut entries) = self.0.lock() {
                entries.remove(key);
            }
            Box::pin(async { Ok(()) })
        }

        fn clear(&self) -> BoxFuture<'_, cache::Result<()>> {
            if let Ok(mut entries) = self.0.lock() {
                entries.clear();
            }
            Box::pin(async { Ok(()) })
        }
    }

    fn repo(name: &str) -> ActionRepoId {
        ActionRepoId::new(Some(HOST.to_string()), "o", name, None)
    }

    fn key(name: &str) -> String {
        repo(name).cache_key("github", HOST)
    }

    fn cached_versions() -> version::Set {
        let info = Arc::new(Info::new(Version::parse("v1.0.0"), CACHED_SHA));
        version::Set::build(
            vec![Arc::clone(&info)],
            HashMap::from([(1, Arc::clone(&info))]),
            HashMap::new(),
            Some(info),
        )
        .with_default_branch(BranchInfo::new("main", CACHED_SHA))
    }

    fn day_old_entry() -> Result<Entry, Box<dyn std::error::Error>> {
        let then = SystemTime::now()
            .checked_sub(DAY_AGO)
            .ok_or("clock before epoch")?;
        Ok(Entry::with_clock(cached_versions(), &Past(then)))
    }

    fn etags(tags: &str, repo: &str, branch: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("github.tags".to_string(), tags.to_string()),
            ("github.repo".to_string(), repo.to_string()),
            ("github.branch".to_string(), branch.to_string()),
        ])
    }

    fn fetcher(
        server: &MockServer,
        cache: Arc<MemoryCache>,
    ) -> Result<BatchFetcher<MemoryCache>, Box<dyn std::error::Error>> {
        let provider = GitHubProvider::with_host(&server.uri(), HOST, None)?;
        Ok(BatchFetcher::rest_only(
            provider,
            cache,
            std::num::NonZeroU8::MIN.saturating_add(9),
        ))
    }

    async fn answer(
        server: &MockServer,
        url: &str,
        etag: Option<&str>,
        response: ResponseTemplate,
    ) {
        let mock = Mock::given(method("GET")).and(path(url.to_string()));
        match etag {
            Some(etag) => {
                mock.and(header("if-none-match", etag))
                    .respond_with(response)
                    .with_priority(1)
                    .mount(server)
                    .await;
            }
            None => mock.respond_with(response).mount(server).await,
        }
    }

    async fn fetch_one(
        server: &MockServer,
        cache: &Arc<MemoryCache>,
        name: &str,
    ) -> Result<actup_core::Result<version::Set>, Box<dyn std::error::Error>> {
        let fetcher = fetcher(server, Arc::clone(cache))?;
        let mut results = fetcher
            .fetch_versions(&[repo(name)], &actup_core::fetch::Silent)
            .await;
        Ok(results.remove(&repo(name)).ok_or("no result")?)
    }

    fn latest(versions: &version::Set) -> Option<String> {
        versions.latest().map(|t| t.version().to_string())
    }

    #[tokio::test]
    async fn three_not_modified_answers_renew_the_expired_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let not_modified = || ResponseTemplate::new(304);
        answer(&server, "/repos/o/r/tags", Some("\"t1\""), not_modified()).await;
        answer(&server, "/repos/o/r", Some("\"r1\""), not_modified()).await;
        answer(
            &server,
            "/repos/o/r/branches/main",
            Some("\"b1\""),
            not_modified(),
        )
        .await;
        let cache = MemoryCache::with(
            &key("r"),
            day_old_entry()?.with_validators(etags("\"t1\"", "\"r1\"", "\"b1\"")),
        );

        let versions = fetch_one(&server, &cache, "r").await??;

        assert_eq!(versions.stale(), None);
        assert_eq!(latest(&versions), Some("v1.0.0".to_string()));
        let entry = cache.entry(&key("r")).ok_or("entry gone")?;
        assert!(!entry.is_expired(Duration::from_hours(1)));
        assert_eq!(entry.validators(), &etags("\"t1\"", "\"r1\"", "\"b1\""));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_changed_call_refetches_the_unchanged_ones_and_stores_new_etags()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        answer(
            &server,
            "/repos/o/r/tags",
            Some("\"t1\""),
            ResponseTemplate::new(200)
                .insert_header("etag", "\"t2\"")
                .set_body_string(TAGS_RESPONSE),
        )
        .await;
        answer(
            &server,
            "/repos/o/r",
            Some("\"r1\""),
            ResponseTemplate::new(304),
        )
        .await;
        answer(
            &server,
            "/repos/o/r/branches/main",
            Some("\"b1\""),
            ResponseTemplate::new(304),
        )
        .await;
        answer(
            &server,
            "/repos/o/r",
            None,
            ResponseTemplate::new(200)
                .insert_header("etag", "\"r2\"")
                .set_body_string(REPO_RESPONSE),
        )
        .await;
        answer(
            &server,
            "/repos/o/r/branches/main",
            None,
            ResponseTemplate::new(200)
                .insert_header("etag", "\"b2\"")
                .set_body_string(BRANCH_RESPONSE),
        )
        .await;
        let cache = MemoryCache::with(
            &key("r"),
            day_old_entry()?.with_validators(etags("\"t1\"", "\"r1\"", "\"b1\"")),
        );

        let versions = fetch_one(&server, &cache, "r").await??;

        assert_eq!(versions.stale(), None);
        assert_eq!(latest(&versions), Some("v6.0.2".to_string()));
        assert_eq!(
            versions.default_branch().map(BranchInfo::sha),
            Some("0c366fd6a839edf440554fa01a7085ccba70ac98")
        );
        let entry = cache.entry(&key("r")).ok_or("entry gone")?;
        assert_eq!(entry.validators(), &etags("\"t2\"", "\"r2\"", "\"b2\""));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            5
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_renamed_default_branch_is_fetched_without_the_old_branch_etag()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let ok = |etag: &str, body: &str| {
            ResponseTemplate::new(200)
                .insert_header("etag", etag)
                .set_body_string(body)
        };
        answer(
            &server,
            "/repos/o/r/tags",
            Some("\"t1\""),
            ResponseTemplate::new(304),
        )
        .await;
        answer(
            &server,
            "/repos/o/r/tags",
            None,
            ok("\"t2\"", TAGS_RESPONSE),
        )
        .await;
        answer(
            &server,
            "/repos/o/r",
            Some("\"r1\""),
            ok(
                "\"r2\"",
                r#"{"full_name": "o/r", "default_branch": "trunk"}"#,
            ),
        )
        .await;
        answer(
            &server,
            "/repos/o/r/branches/trunk",
            None,
            ok("\"b2\"", BRANCH_RESPONSE),
        )
        .await;
        let cache = MemoryCache::with(
            &key("r"),
            day_old_entry()?.with_validators(etags("\"t1\"", "\"r1\"", "\"b1\"")),
        );

        let versions = fetch_one(&server, &cache, "r").await??;

        assert_eq!(
            versions.default_branch().map(BranchInfo::name),
            Some("trunk")
        );
        assert_eq!(latest(&versions), Some("v6.0.2".to_string()));
        let entry = cache.entry(&key("r")).ok_or("entry gone")?;
        assert_eq!(entry.validators(), &etags("\"t2\"", "\"r2\"", "\"b2\""));
        let requests = server.received_requests().await.unwrap_or_default();
        let paths: Vec<&str> = requests.iter().map(|request| request.url.path()).collect();
        assert_eq!(
            paths,
            [
                "/repos/o/r/tags",
                "/repos/o/r",
                "/repos/o/r/branches/trunk",
                "/repos/o/r/tags"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_fetch_without_stored_etags_sends_none_and_stores_the_answered_ones()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let ok = |etag: &str, body: &str| {
            ResponseTemplate::new(200)
                .insert_header("etag", etag)
                .set_body_string(body)
        };
        answer(
            &server,
            "/repos/o/r/tags",
            None,
            ok("\"t1\"", TAGS_RESPONSE),
        )
        .await;
        answer(&server, "/repos/o/r", None, ok("\"r1\"", REPO_RESPONSE)).await;
        answer(
            &server,
            "/repos/o/r/branches/main",
            None,
            ok("\"b1\"", BRANCH_RESPONSE),
        )
        .await;
        let cache = MemoryCache::with(&key("r"), day_old_entry()?);

        let versions = fetch_one(&server, &cache, "r").await??;

        assert_eq!(latest(&versions), Some("v6.0.2".to_string()));
        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| !request.headers.contains_key("if-none-match"))
        );
        let entry = cache.entry(&key("r")).ok_or("entry gone")?;
        assert_eq!(entry.validators(), &etags("\"t1\"", "\"r1\"", "\"b1\""));
        Ok(())
    }

    #[tokio::test]
    async fn a_rate_limit_answers_with_the_expired_entry_marked_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let reset = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
            + 720;
        answer(
            &server,
            "/repos/o/limited/tags",
            None,
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.to_string())
                .set_body_string(r#"{"message": "API rate limit exceeded"}"#),
        )
        .await;
        answer(
            &server,
            "/repos/o/throttled/tags",
            None,
            ResponseTemplate::new(429).insert_header("retry-after", "60"),
        )
        .await;
        let cache = MemoryCache::with(&key("limited"), day_old_entry()?);
        cache.set(&key("throttled"), day_old_entry()?).await?;

        let limited = fetch_one(&server, &cache, "limited").await??;
        let throttled = fetch_one(&server, &cache, "throttled").await??;

        let stale = limited.stale().ok_or("limited not stale")?;
        assert_eq!(stale.reason(), StaleReason::RateLimited);
        assert!(stale.age() >= DAY_AGO);
        let retry = stale.retry_after().ok_or("no reset")?;
        assert!(
            (Duration::from_mins(11)..=Duration::from_mins(12)).contains(&retry),
            "{retry:?}"
        );
        assert_eq!(latest(&limited), Some("v1.0.0".to_string()));
        assert_eq!(
            throttled.stale().map(|s| (s.reason(), s.retry_after())),
            Some((StaleReason::RateLimited, Some(Duration::from_mins(1))))
        );
        let entry = cache.entry(&key("limited")).ok_or("entry gone")?;
        assert!(entry.is_expired(Duration::from_hours(1)));
        Ok(())
    }

    #[tokio::test]
    async fn a_network_failure_answers_with_the_expired_entry_marked_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        answer(
            &server,
            "/repos/o/r/tags",
            None,
            ResponseTemplate::new(502).set_body_string("bad gateway"),
        )
        .await;
        let cache = MemoryCache::with(&key("r"), day_old_entry()?);

        let versions = fetch_one(&server, &cache, "r").await??;

        let stale = versions.stale().ok_or("not stale")?;
        assert_eq!(
            (stale.reason(), stale.retry_after()),
            (StaleReason::Network, None)
        );
        assert_eq!(latest(&versions), Some("v1.0.0".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn failures_without_an_entry_or_of_the_user_stay_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        answer(
            &server,
            "/repos/o/limited/tags",
            None,
            ResponseTemplate::new(429).insert_header("retry-after", "60"),
        )
        .await;
        mount_gone(&server).await;
        let cache = MemoryCache::with(&key("gone"), day_old_entry()?);

        let limited = fetch_one(&server, &cache, "limited").await?;
        let gone = fetch_one(&server, &cache, "gone").await?;

        assert!(
            matches!(limited, Err(actup_core::Error::RateLimited { .. })),
            "{limited:?}"
        );
        assert!(
            matches!(gone, Err(actup_core::Error::NotFound { .. })),
            "{gone:?}"
        );
        Ok(())
    }

    async fn mount_gone(server: &MockServer) {
        answer(
            server,
            "/repos/o/gone/tags",
            None,
            ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#),
        )
        .await;
    }

    #[tokio::test]
    async fn a_github_host_without_a_batch_config_falls_back_to_its_expired_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        answer(
            &server,
            "/repos/o/r/tags",
            None,
            ResponseTemplate::new(429).insert_header("retry-after", "60"),
        )
        .await;
        let mut registry = provider::Registry::new();
        registry.set_default_host(HOST);
        registry.register(Box::new(GitHubProvider::with_host(
            &server.uri(),
            HOST,
            None,
        )?));
        let cache = MemoryCache::with(&key("r"), day_old_entry()?);

        let results = actup_provider_github::fetch_versions_with_registry(
            &[repo("r")],
            &actup_core::Config::default(),
            &registry,
            cache,
            &actup_core::fetch::Silent,
        )
        .await;

        let versions = results
            .get(&repo("r"))
            .ok_or("no result")?
            .as_ref()
            .map_err(ToString::to_string)?;
        assert_eq!(
            versions.stale().map(|s| (s.reason(), s.retry_after())),
            Some((StaleReason::RateLimited, Some(Duration::from_mins(1))))
        );
        assert_eq!(latest(versions), Some("v1.0.0".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn a_github_host_without_a_batch_config_answers_a_fresh_entry_from_the_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        let mut registry = provider::Registry::new();
        registry.set_default_host(HOST);
        registry.register(Box::new(GitHubProvider::with_host(
            &server.uri(),
            HOST,
            None,
        )?));
        let cache = MemoryCache::with(&key("r"), Entry::new(cached_versions()));

        let results = actup_provider_github::fetch_versions_with_registry(
            &[repo("r")],
            &actup_core::Config::default(),
            &registry,
            cache,
            &actup_core::fetch::Silent,
        )
        .await;

        let versions = results
            .get(&repo("r"))
            .ok_or("no result")?
            .as_ref()
            .map_err(ToString::to_string)?;
        assert_eq!(versions.stale(), None);
        assert_eq!(latest(versions), Some("v1.0.0".to_string()));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            0
        );
        Ok(())
    }

    struct RateLimitedProvider;

    impl Provider for RateLimitedProvider {
        fn kind(&self) -> Kind {
            Kind::GitLab
        }

        fn host(&self) -> &'static str {
            "gitlab.invalid"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            Box::pin(async {
                Err(actup_core::Error::RateLimited {
                    provider: "gitlab".to_string(),
                    retry_after: None,
                })
            })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            Box::pin(async {
                Err(actup_core::Error::ProviderNotFound {
                    host: String::new(),
                })
            })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    struct ListingProvider(Arc<AtomicUsize>);

    struct StaleListingProvider(Arc<AtomicUsize>);

    impl Provider for StaleListingProvider {
        fn kind(&self) -> Kind {
            Kind::GitLab
        }

        fn host(&self) -> &'static str {
            "gitlab.invalid"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let set = cached_versions();
            let set = match cache::Staleness::after(
                &actup_core::Error::RateLimited {
                    provider: "gitlab".to_string(),
                    retry_after: None,
                },
                DAY_AGO,
            ) {
                Some(stale) => set.into_stale(stale),
                None => set,
            };
            Box::pin(async move { Ok(set) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            Box::pin(async {
                Err(actup_core::Error::ProviderNotFound {
                    host: String::new(),
                })
            })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn a_provider_without_batching_keeps_a_stale_set_out_of_the_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = provider::Registry::new();
        registry.set_default_host("gitlab.invalid");
        registry.register(Box::new(StaleListingProvider(Arc::clone(&calls))));
        let repo = ActionRepoId::new(Some("gitlab.invalid".to_string()), "g", "p", None);
        let provider = registry.find_for_repo(&repo).ok_or("no provider")?;
        let key = repo.cache_key(provider.cache_namespace(), provider.host());
        let cache = Arc::new(MemoryCache::default());

        for _ in 0..2 {
            let results = actup_provider_github::fetch_versions_with_registry(
                std::slice::from_ref(&repo),
                &actup_core::Config::default(),
                &registry,
                Arc::clone(&cache),
                &actup_core::fetch::Silent,
            )
            .await;
            let versions = results
                .get(&repo)
                .ok_or("no result")?
                .as_ref()
                .map_err(ToString::to_string)?;
            assert_eq!(
                versions.stale().map(|s| s.reason()),
                Some(StaleReason::RateLimited)
            );
        }

        assert!(cache.entry(&key).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    impl Provider for ListingProvider {
        fn kind(&self) -> Kind {
            Kind::GitLab
        }

        fn host(&self) -> &'static str {
            "gitlab.invalid"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(cached_versions()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            Box::pin(async {
                Err(actup_core::Error::ProviderNotFound {
                    host: String::new(),
                })
            })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn a_provider_without_batching_stores_its_versions_under_its_cache_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = provider::Registry::new();
        registry.set_default_host("gitlab.invalid");
        registry.register(Box::new(ListingProvider(Arc::clone(&calls))));
        let repo = ActionRepoId::new(Some("gitlab.invalid".to_string()), "g", "p", None);
        let provider = registry.find_for_repo(&repo).ok_or("no provider")?;
        let key = repo.cache_key(provider.cache_namespace(), provider.host());
        let cache = Arc::new(MemoryCache::default());

        for _ in 0..2 {
            let results = actup_provider_github::fetch_versions_with_registry(
                std::slice::from_ref(&repo),
                &actup_core::Config::default(),
                &registry,
                Arc::clone(&cache),
                &actup_core::fetch::Silent,
            )
            .await;
            let versions = results
                .get(&repo)
                .ok_or("no result")?
                .as_ref()
                .map_err(ToString::to_string)?;
            assert_eq!(latest(versions), Some("v1.0.0".to_string()));
        }

        let entry = cache
            .entry(&key)
            .ok_or("no entry under the provider's cache key")?;
        assert_eq!(latest(entry.versions()), Some("v1.0.0".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_provider_without_batching_falls_back_to_its_expired_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut registry = provider::Registry::new();
        registry.set_default_host("gitlab.invalid");
        registry.register(Box::new(RateLimitedProvider));
        let repo = ActionRepoId::new(Some("gitlab.invalid".to_string()), "g", "p", None);
        let cache = MemoryCache::with(
            &repo.cache_key(RateLimitedProvider.cache_namespace(), "gitlab.invalid"),
            day_old_entry()?,
        );

        let results = actup_provider_github::fetch_versions_with_registry(
            std::slice::from_ref(&repo),
            &actup_core::Config::default(),
            &registry,
            cache,
            &actup_core::fetch::Silent,
        )
        .await;

        let versions = results
            .get(&repo)
            .ok_or("no result")?
            .as_ref()
            .map_err(ToString::to_string)?;
        assert_eq!(
            versions.stale().map(|s| s.reason()),
            Some(StaleReason::RateLimited)
        );
        assert_eq!(latest(versions), Some("v1.0.0".to_string()));
        Ok(())
    }
}

async fn serve_body(server: &MockServer, url: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(url.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn minimal_rest_responses_parse() -> Result<(), Box<dyn std::error::Error>> {
    let (tagged, tag_object, head) = ("a".repeat(40), "c".repeat(40), "d".repeat(40));
    let server = MockServer::start().await;
    serve_body(
        &server,
        "/repos/o/r/tags",
        format!(r#"[{{"name": "v1.0.0", "commit": {{"sha": "{tagged}"}}}}]"#),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r",
        r#"{"default_branch": "main"}"#.into(),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r/branches/main",
        format!(r#"{{"commit": {{"sha": "{head}"}}}}"#),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r/git/ref/tags/v1.0.0",
        format!(r#"{{"object": {{"type": "tag", "sha": "{tag_object}"}}}}"#),
    )
    .await;
    serve_body(
        &server,
        &format!("/repos/o/r/git/tags/{tag_object}"),
        format!(r#"{{"object": {{"sha": "{tagged}"}}}}"#),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r/contents/action.yml",
        r#"{"content": "bmFtZTogTWluaW1hbAo=", "encoding": "base64"}"#.into(),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r/commits/abc1234",
        format!(r#"{{"sha": "{FULL_SHA}"}}"#),
    )
    .await;
    mount_not_found(&server, "/repos/o/r/git/ref/tags/abc1234").await;
    mount_not_found(&server, "/repos/o/r/branches/abc1234").await;
    let provider = provider(&server)?;
    let repo = ActionRepoId::simple("o", "r");

    let set = provider.versions_via_rest(&repo).await?;
    assert_eq!(set.tag_sha("v1.0.0"), Some(tagged.as_str()));
    assert_eq!(
        set.default_branch()
            .map(actup_core::version::BranchInfo::sha),
        Some(head.as_str())
    );
    assert_eq!(
        provider.resolve_ref_to_sha(&repo, "v1.0.0").await?.as_str(),
        tagged
    );
    assert_eq!(
        provider.resolve_ref_to_sha(&repo, "main").await?.as_str(),
        head
    );
    assert_eq!(
        provider
            .resolve_ref_to_sha(&repo, "abc1234")
            .await?
            .as_str(),
        FULL_SHA
    );
    let action = provider.get_action_yml(&repo, "v1.0.0").await?;
    assert_eq!(action.name(), Some("Minimal"));
    Ok(())
}

#[tokio::test]
async fn refs_are_percent_encoded_in_rest_urls() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let shas: Vec<String> = ('a'..='d').map(|c| c.to_string().repeat(40)).collect();
    serve_body(
        &server,
        "/repos/o/r/git/ref/tags/v1.0.0%2Bbuild",
        format!(
            r#"{{"object": {{"type": "commit", "sha": "{}"}}}}"#,
            shas[0]
        ),
    )
    .await;
    for (branch, sha) in [
        ("feat%231", &shas[1]),
        ("a%25b", &shas[2]),
        ("what%3F", &shas[3]),
    ] {
        serve_body(
            &server,
            &format!("/repos/o/r/branches/{branch}"),
            format!(r#"{{"commit": {{"sha": "{sha}"}}}}"#),
        )
        .await;
    }
    Mock::given(method("GET"))
        .and(path("/repos/o/r/contents/action.yml"))
        .and(query_param("ref", "v1.0.0+build"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"content": "bmFtZTogTWluaW1hbAo=", "encoding": "base64"}"#),
        )
        .mount(&server)
        .await;
    let provider = provider(&server)?;
    let repo = ActionRepoId::simple("o", "r");

    for (ref_name, sha) in [
        ("v1.0.0+build", &shas[0]),
        ("feat#1", &shas[1]),
        ("a%b", &shas[2]),
        ("what?", &shas[3]),
    ] {
        assert_eq!(
            provider.resolve_ref_to_sha(&repo, ref_name).await?.as_str(),
            sha.as_str(),
            "{ref_name}"
        );
    }
    let action = provider.get_action_yml(&repo, "v1.0.0+build").await?;
    assert_eq!(action.name(), Some("Minimal"));
    Ok(())
}

#[tokio::test]
async fn slashes_survive_encoding_in_rest_urls() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let (tagged, branched) = ("a".repeat(40), "b".repeat(40));
    serve_body(
        &server,
        "/repos/o/r/git/ref/tags/release/v1",
        format!(r#"{{"object": {{"type": "commit", "sha": "{tagged}"}}}}"#),
    )
    .await;
    serve_body(
        &server,
        "/repos/o/r/branches/feature/x",
        format!(r#"{{"commit": {{"sha": "{branched}"}}}}"#),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/contents/sub/dir/action.yml"))
        .and(query_param("ref", "release/v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"content": "bmFtZTogTWluaW1hbAo=", "encoding": "base64"}"#),
        )
        .mount(&server)
        .await;
    let provider = provider(&server)?;
    let repo = ActionRepoId::simple("o", "r");

    assert_eq!(
        provider
            .resolve_ref_to_sha(&repo, "release/v1")
            .await?
            .as_str(),
        tagged
    );
    assert_eq!(
        provider
            .resolve_ref_to_sha(&repo, "feature/x")
            .await?
            .as_str(),
        branched
    );
    let nested = ActionRepoId::new(None, "o", "r", Some("sub/dir".into()));
    let action = provider.get_action_yml(&nested, "release/v1").await?;
    assert_eq!(action.name(), Some("Minimal"));
    Ok(())
}

#[tokio::test]
async fn dot_segments_are_refused_before_any_request() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let provider = provider(&server)?;
    let repo = ActionRepoId::simple("o", "r");

    for ref_name in ["../../x/y", "./x", "..", "a/./b"] {
        let result = provider.resolve_ref_to_sha(&repo, ref_name).await;
        assert!(
            matches!(result, Err(actup_core::Error::InvalidRef { .. })),
            "{ref_name}: {result:?}"
        );
    }
    let nested = ActionRepoId::new(None, "o", "r", Some("../x".into()));
    let action = provider.get_action_yml(&nested, "v1").await;
    assert!(
        matches!(action, Err(actup_core::Error::InvalidRef { .. })),
        "{action:?}"
    );
    assert_eq!(server.received_requests().await.map(|r| r.len()), Some(0));
    Ok(())
}
