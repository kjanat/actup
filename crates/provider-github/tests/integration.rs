//! Integration tests for GitHub provider with mocked HTTP responses

#![allow(clippy::expect_used, clippy::unwrap_used)]

use actup_provider_github::GitHubClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
