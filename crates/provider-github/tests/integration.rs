//! Integration tests for GitHub provider with mocked HTTP responses

#![allow(clippy::expect_used, clippy::unwrap_used)]

use actup_core::{ActionRepoId, Provider};
use actup_provider_github::{GitHubClient, GitHubProvider};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const TAGS_RESPONSE: &str = include_str!("fixtures/tags_response.json");
const REPO_RESPONSE: &str = include_str!("fixtures/repo_response.json");
const BRANCH_RESPONSE: &str = include_str!("fixtures/branch_response.json");

#[tokio::test]
async fn test_list_tags() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TAGS_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let tags = client
        .list_tags("actions", "checkout", None)
        .await
        .expect("failed to list tags");

    assert_eq!(tags.len(), 5);
    assert_eq!(tags[0].name, "v6.0.2");
    assert_eq!(
        tags[0].commit.sha,
        "de0fac2e4500dabe0009e67214ff5f5447ce83dd"
    );
}

#[tokio::test]
async fn test_list_tags_limited_stops_after_requested_count() {
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

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let tags = client
        .list_tags_limited("actions", "checkout", 2)
        .await
        .expect("failed to list limited tags");

    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].name, "v6.0.2");
    assert_eq!(tags[1].name, "v6.0.1");
}

#[tokio::test]
async fn test_get_repo() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout"))
        .respond_with(ResponseTemplate::new(200).set_body_string(REPO_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let repo = client
        .get_repo("actions", "checkout")
        .await
        .expect("failed to get repo");

    assert_eq!(repo.full_name, "actions/checkout");
    assert_eq!(repo.default_branch, "main");
}

#[tokio::test]
async fn test_get_branch() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/actions/checkout/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BRANCH_RESPONSE))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let branch = client
        .get_branch("actions", "checkout", "main")
        .await
        .expect("failed to get branch");

    assert_eq!(branch.name, "main");
    assert_eq!(
        branch.commit.sha,
        "0c366fd6a839edf440554fa01a7085ccba70ac98"
    );
}

#[tokio::test]
async fn test_not_found_error() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/nonexistent/repo"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message": "Not Found"}"#))
        .mount(&mock_server)
        .await;

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let result = client.get_repo("nonexistent", "repo").await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, actup_core::Error::NotFound { .. }));
}

#[tokio::test]
async fn test_rate_limit_error() {
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

    let client = GitHubClient::with_api_base(&mock_server.uri(), "github.com", None)
        .expect("failed to create client");

    let result = client.get_repo("actions", "checkout").await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, actup_core::Error::RateLimited { .. }));
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
        config::{ApiMode, HostConfig, HostKind},
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
        HostConfig {
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
    let fetcher = BatchFetcher::with_graphql(provider, Arc::new(cache::Noop), Some("token"), 10);
    let gone = ActionRepoId::new(Some("ghe.invalid".to_string()), "o", "gone", None);

    let results = fetcher.fetch_versions(std::slice::from_ref(&gone)).await;

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
