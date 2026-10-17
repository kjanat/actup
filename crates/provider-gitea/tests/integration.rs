//! Gitea provider tests against a mocked REST API.

use std::num::NonZeroU8;

use actup_core::{
    ActionRepoId, ApiMode, Error, HostKind, Provider, config::registration::ProviderConfig,
};
use actup_provider_gitea::{CODEBERG_API_BASE, CODEBERG_HOST, GiteaProvider, REGISTRATION};
use base64::{Engine, engine::general_purpose::STANDARD};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const REPO: &str = "/api/v1/repos/o/r";
const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TAG_OBJECT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn repo() -> ActionRepoId {
    ActionRepoId::simple("o", "r")
}

fn provider(server: &MockServer) -> actup_core::Result<GiteaProvider> {
    GiteaProvider::new(&format!("{}/api/v1", server.uri()), "codeberg.org", None)
}

async fn get(server: &MockServer, url: &str, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(url))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

fn tag(name: &str, commit: &str) -> String {
    format!(
        r#"{{"name": "{name}", "id": "{TAG_OBJECT}", "commit": {{"sha": "{commit}", "url": "u"}}, "message": "release", "tarball_url": null, "zipball_url": null}}"#
    )
}

fn repository(default_branch: &str) -> String {
    format!(
        r#"{{"id": 1, "name": "r", "full_name": "o/r", "description": null, "empty": false, "private": false, "fork": false, "default_branch": "{default_branch}", "clone_url": "c", "ssh_url": "s", "html_url": "h", "owner": {{"id": 2, "login": "o", "full_name": null, "email": null, "avatar_url": "a"}}}}"#
    )
}

fn branch(name: &str, commit: &str) -> String {
    format!(
        r#"{{"name": "{name}", "commit": {{"id": "{commit}", "url": "u"}}, "protected": false}}"#
    )
}

fn content(name: &str, text: &str) -> String {
    format!(
        r#"{{"type": "file", "encoding": "base64", "size": {}, "name": "{name}", "path": "{name}", "content": "{}", "sha": "s", "url": "u", "html_url": "h", "git_url": "g", "download_url": null}}"#,
        text.len(),
        STANDARD.encode(text)
    )
}

fn commit(sha: &str) -> String {
    let author = r#"{"name": "a", "email": "a@example.com", "date": "2026-01-01T00:00:00Z"}"#;
    format!(
        r#"{{"sha": "{sha}", "url": "u", "html_url": "h", "commit": {{"message": "m", "author": {author}, "committer": {author}}}}}"#
    )
}

async fn serve_repository(server: &MockServer) {
    get(server, REPO, 200, &repository("main")).await;
    get(
        server,
        &format!("{REPO}/branches/main"),
        200,
        &branch("main", HEAD),
    )
    .await;
}

async fn versions_error(status: u16, body: &str) -> Result<Error, Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    get(&server, &format!("{REPO}/tags"), status, body).await;
    match provider(&server)?.get_versions(&repo()).await {
        Ok(set) => Err(format!("expected an error, got {set:?}").into()),
        Err(e) => Ok(e),
    }
}

async fn resolve_error(
    server: &MockServer,
    ref_name: &str,
) -> Result<Error, Box<dyn std::error::Error>> {
    match provider(server)?
        .resolve_ref_to_sha(&repo(), ref_name)
        .await
    {
        Ok(sha) => Err(format!("expected an error, got {sha:?}").into()),
        Err(e) => Ok(e),
    }
}

#[test]
fn registration_declares_the_codeberg_preset() {
    assert_eq!(REGISTRATION.kind, HostKind::Gitea);
    let [preset] = REGISTRATION.presets else {
        panic!("expected one preset: {:?}", REGISTRATION.presets);
    };
    assert_eq!(
        (preset.host, preset.api_base, preset.token_env),
        (CODEBERG_HOST, CODEBERG_API_BASE, "CODEBERG_TOKEN")
    );
    assert_eq!(REGISTRATION.default_token_env, Some("GITEA_TOKEN"));
}

#[test]
fn registration_infers_the_api_root_of_other_hosts() {
    assert_eq!((REGISTRATION.infer_api_base)(CODEBERG_HOST), None);
    assert_eq!(
        (REGISTRATION.infer_api_base)("forgejo.example.com").as_deref(),
        Some("https://forgejo.example.com/api/v1")
    );
}

#[test]
fn registration_supports_rest_only() {
    assert!(REGISTRATION.capabilities.supports_rest);
    assert!(!REGISTRATION.capabilities.supports_graphql);
}

#[test]
fn registration_creates_a_provider_for_the_host() -> TestResult {
    let provider = (REGISTRATION.create)(ProviderConfig {
        host: "forgejo.example.com".into(),
        kind: HostKind::Gitea,
        api_base: "https://forgejo.example.com/api/v1".into(),
        token_env: Some("_ACTUP_TEST_UNSET_GITEA_TOKEN".into()),
        api_mode: ApiMode::Rest,
        max_tags_per_repo: NonZeroU8::MIN,
    })?;
    assert_eq!(provider.host(), "forgejo.example.com");
    assert_eq!(provider.kind(), actup_core::provider::Kind::Gitea);
    Ok(())
}

#[tokio::test]
async fn get_versions_reads_tags_and_the_default_branch() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/tags"),
        200,
        &format!("[{}, {}]", tag("v1.2.0", COMMIT_B), tag("v1.1.0", COMMIT_A)),
    )
    .await;
    serve_repository(&server).await;

    let set = provider(&server)?.get_versions(&repo()).await?;

    let latest = set.latest().ok_or("no latest")?;
    assert_eq!(
        (latest.version().as_str(), latest.sha()),
        ("v1.2.0", COMMIT_B)
    );
    assert_eq!(set.tag_sha("v1.1.0"), Some(COMMIT_A));
    let default = set.default_branch().ok_or("no default branch")?;
    assert_eq!((default.name(), default.sha()), ("main", HEAD));
    Ok(())
}

async fn serve_tag_page(server: &MockServer, page: u32, limit: &str, names: &[String]) {
    let body: Vec<String> = names.iter().map(|name| tag(name, COMMIT_A)).collect();
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/tags")))
        .and(query_param("page", page.to_string()))
        .and(query_param("limit", limit))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("[{}]", body.join(","))))
        .expect(1)
        .mount(server)
        .await;
}

fn patches(minor: u32, count: u32) -> Vec<String> {
    (0..count)
        .map(|patch| format!("v1.{minor}.{patch}"))
        .collect()
}

fn max_tags(n: u8) -> Result<NonZeroU8, &'static str> {
    NonZeroU8::new(n).ok_or("zero")
}

#[tokio::test]
async fn get_versions_reads_the_next_page_after_a_full_page() -> TestResult {
    let server = MockServer::start().await;
    serve_tag_page(&server, 1, "100", &patches(0, 100)).await;
    serve_tag_page(&server, 2, "100", &["v2.0.0".to_string()]).await;
    serve_repository(&server).await;

    let set = provider(&server)?
        .max_tags(max_tags(150)?)
        .get_versions(&repo())
        .await?;

    assert_eq!(set.semver_tags().len(), 101);
    assert_eq!(
        set.latest().map(|latest| latest.version().as_str()),
        Some("v2.0.0")
    );
    Ok(())
}

#[tokio::test]
async fn get_versions_stops_at_max_tags() -> TestResult {
    let server = MockServer::start().await;
    serve_tag_page(&server, 1, "100", &patches(0, 100)).await;
    serve_tag_page(&server, 2, "100", &patches(1, 100)).await;
    serve_repository(&server).await;

    let set = provider(&server)?
        .max_tags(max_tags(150)?)
        .get_versions(&repo())
        .await?;

    assert_eq!(set.semver_tags().len(), 150);
    assert_eq!(set.tag_sha("v1.1.49"), Some(COMMIT_A));
    assert_eq!(set.tag_sha("v1.1.50"), None);
    Ok(())
}

#[tokio::test]
async fn registration_passes_max_tags_per_repo() -> TestResult {
    let server = MockServer::start().await;
    serve_tag_page(
        &server,
        1,
        "1",
        &["v2.0.0".to_string(), "v1.0.0".to_string()],
    )
    .await;
    serve_repository(&server).await;
    let provider = (REGISTRATION.create)(ProviderConfig {
        host: "codeberg.org".into(),
        kind: HostKind::Gitea,
        api_base: format!("{}/api/v1", server.uri()),
        token_env: Some("_ACTUP_TEST_UNSET_GITEA_TOKEN".into()),
        api_mode: ApiMode::Rest,
        max_tags_per_repo: NonZeroU8::MIN,
    })?;

    let set = provider.get_versions(&repo()).await?;

    let names: Vec<&str> = set
        .semver_tags()
        .iter()
        .map(|t| t.version().as_str())
        .collect();
    assert_eq!(names, ["v2.0.0"]);
    Ok(())
}

#[tokio::test]
async fn get_versions_of_a_missing_repository_is_not_found() -> TestResult {
    let error = versions_error(404, r#"{"message": "repository does not exist"}"#).await?;
    assert!(
        matches!(&error, Error::NotFound { owner, repo, host: Some(host) }
            if owner == "o" && repo == "r" && host == "codeberg.org"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_is_an_auth_error_with_the_message() -> TestResult {
    let error = versions_error(401, r#"{"message": "token is required"}"#).await?;
    assert!(
        matches!(&error, Error::Auth { provider, reason, .. }
            if provider == "gitea" && reason == "token is required"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn forbidden_without_a_message_names_the_status() -> TestResult {
    let error = versions_error(403, "{}").await?;
    assert!(
        matches!(&error, Error::Auth { reason, .. } if reason.contains("403")),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn too_many_requests_is_rate_limited() -> TestResult {
    let error = versions_error(429, "").await?;
    assert!(
        matches!(&error, Error::RateLimited { provider, retry_after: None } if provider == "gitea"),
        "{error:?}"
    );
    assert!(error.is_environment_error());
    Ok(())
}

#[tokio::test]
async fn a_server_error_is_a_network_error() -> TestResult {
    let error = versions_error(500, "boom").await?;
    assert!(
        matches!(&error, Error::Network { message, .. } if message.contains("500")),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_unparseable_body_is_an_internal_error() -> TestResult {
    let error = versions_error(200, "[{").await?;
    assert!(matches!(error, Error::Internal { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn resolve_an_annotated_tag_to_its_commit() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/tags/v1.2.0"),
        200,
        &tag("v1.2.0", COMMIT_B),
    )
    .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&repo(), "v1.2.0")
        .await?;

    assert_eq!(sha.as_str(), COMMIT_B);
    Ok(())
}

#[tokio::test]
async fn resolve_a_branch_after_the_tag_lookup_misses() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/branches/release/v1"),
        200,
        &branch("release/v1", HEAD),
    )
    .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&repo(), "release/v1")
        .await?;

    assert_eq!(sha.as_str(), HEAD);
    Ok(())
}

#[tokio::test]
async fn resolve_encodes_a_hash_in_the_ref() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/branches/feat%231"),
        200,
        &branch("feat#1", HEAD),
    )
    .await;
    get(
        &server,
        &format!("{REPO}/branches/feat"),
        200,
        &branch("feat", COMMIT_A),
    )
    .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&repo(), "feat#1")
        .await?;

    assert_eq!(sha.as_str(), HEAD);
    Ok(())
}

#[tokio::test]
async fn get_action_yml_encodes_a_plus_in_the_ref() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/contents/action.yml")))
        .and(query_param("ref", "v1.0.0+build"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(content("action.yml", "name: Plus\n")),
        )
        .mount(&server)
        .await;

    let action = provider(&server)?
        .get_action_yml(&repo(), "v1.0.0+build")
        .await?;

    assert_eq!(action.name(), Some("Plus"));
    Ok(())
}

#[tokio::test]
async fn resolve_a_short_sha_through_the_commit() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/git/commits/aaaaaaa"),
        200,
        &commit(COMMIT_A),
    )
    .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&repo(), "aaaaaaa")
        .await?;

    assert_eq!(sha.as_str(), COMMIT_A);
    Ok(())
}

#[tokio::test]
async fn resolve_an_unknown_short_sha_is_ref_not_found() -> TestResult {
    let server = MockServer::start().await;

    let error = resolve_error(&server, "eeeeeee").await?;

    assert!(
        matches!(&error, Error::RefNotFound { ref_name, .. } if ref_name == "eeeeeee"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn resolve_stops_at_an_auth_error() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/tags/v1"),
        401,
        r#"{"message": "token is required"}"#,
    )
    .await;

    let error = resolve_error(&server, "v1").await?;

    assert!(matches!(error, Error::Auth { .. }), "{error:?}");
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1);
    Ok(())
}

#[tokio::test]
async fn resolve_rejects_a_sha_that_is_not_40_hex_digits() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/tags/v1"),
        200,
        &tag("v1", "abc123"),
    )
    .await;

    let error = resolve_error(&server, "v1").await?;

    assert!(matches!(error, Error::Internal { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn get_default_branch_reads_the_repository() -> TestResult {
    let server = MockServer::start().await;
    get(&server, REPO, 200, &repository("trunk")).await;

    assert_eq!(
        provider(&server)?.get_default_branch(&repo()).await?,
        "trunk"
    );
    Ok(())
}

#[tokio::test]
async fn get_readme_decodes_the_first_readme_found() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/contents/README.md")))
        .and(query_param("ref", "v1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(content("README.md", "# Hi\n")))
        .mount(&server)
        .await;

    let readme = provider(&server)?.get_readme(&repo(), Some("v1")).await?;

    assert_eq!(readme.as_deref(), Some("# Hi\n"));
    Ok(())
}

#[tokio::test]
async fn get_readme_without_a_readme_is_none() -> TestResult {
    let server = MockServer::start().await;

    assert_eq!(provider(&server)?.get_readme(&repo(), None).await?, None);
    Ok(())
}

#[tokio::test]
async fn get_action_yml_parses_action_yml() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/contents/action.yml"),
        200,
        &content("action.yml", "name: Hello\ndescription: Says hello\n"),
    )
    .await;

    let action = provider(&server)?.get_action_yml(&repo(), "v1").await?;

    assert_eq!(action.name(), Some("Hello"));
    assert_eq!(action.description(), Some("Says hello"));
    Ok(())
}

#[tokio::test]
async fn get_action_yml_falls_back_to_action_yaml() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{REPO}/contents/action.yaml"),
        200,
        &content("action.yaml", "name: Yaml\n"),
    )
    .await;

    let action = provider(&server)?.get_action_yml(&repo(), "v1").await?;

    assert_eq!(action.name(), Some("Yaml"));
    Ok(())
}

#[tokio::test]
async fn get_action_yml_without_a_file_is_not_found() -> TestResult {
    let server = MockServer::start().await;

    let error = provider(&server)?
        .get_action_yml(&repo(), "v1")
        .await
        .err()
        .ok_or("expected an error")?;

    assert!(matches!(error, Error::NotFound { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn a_token_is_sent_in_the_authorization_header() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(REPO))
        .and(header("Authorization", "token gitea-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string(repository("main")))
        .mount(&server)
        .await;

    let provider = GiteaProvider::new(
        &format!("{}/api/v1", server.uri()),
        "codeberg.org",
        Some("gitea-test"),
    )?;

    assert_eq!(provider.get_default_branch(&repo()).await?, "main");
    Ok(())
}

#[tokio::test]
async fn the_api_base_is_the_api_root_as_given() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        "/forgejo/api/v1/repos/o/r",
        200,
        &repository("main"),
    )
    .await;

    let provider = GiteaProvider::new(&format!("{}/forgejo/api/v1/", server.uri()), "fj", None)?;

    assert_eq!(provider.get_default_branch(&repo()).await?, "main");
    Ok(())
}
