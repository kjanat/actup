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
