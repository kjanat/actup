//! GitLab provider tests against a mocked REST API.

use std::{num::NonZeroU8, time::Duration};

use actup_core::{
    ActionRepoId, ApiMode, Error, HostKind, Provider, config::registration::ProviderConfig,
};
use actup_provider_gitlab::{GITLAB_API_BASE, GITLAB_HOST, GitLabProvider, REGISTRATION};
use base64::{Engine, engine::general_purpose::STANDARD};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PROJECT: &str = "/api/v4/projects/o%2Fr";
const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TAG_OBJECT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn repo() -> ActionRepoId {
    ActionRepoId::simple("o", "r")
}

fn provider(server: &MockServer) -> actup_core::Result<GitLabProvider> {
    GitLabProvider::with_host(&format!("{}/api/v4", server.uri()), "gitlab.com", None)
}

async fn get(server: &MockServer, url: &str, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(url))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

fn tag(name: &str, commit: &str) -> String {
    format!(r#"{{"name": "{name}", "target": "{TAG_OBJECT}", "commit": {{"id": "{commit}"}}}}"#)
}

fn project(default_branch: &str) -> String {
    format!(r#"{{"default_branch": "{default_branch}"}}"#)
}

fn branch(commit: &str) -> String {
    format!(r#"{{"commit": {{"id": "{commit}"}}}}"#)
}

fn file(text: &str) -> String {
    format!(
        r#"{{"encoding": "base64", "content": "{}"}}"#,
        STANDARD.encode(text)
    )
}

async fn serve_project(server: &MockServer) {
    get(server, PROJECT, 200, &project("main")).await;
    get(
        server,
        &format!("{PROJECT}/repository/branches/main"),
        200,
        &branch(HEAD),
    )
    .await;
}

async fn serve_tags(server: &MockServer, status: u16, body: &str) {
    get(server, &format!("{PROJECT}/repository/tags"), status, body).await;
}

async fn versions_error(status: u16, body: &str) -> Result<Error, Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    serve_tags(&server, status, body).await;
    match provider(&server)?.get_versions(&repo()).await {
        Ok(set) => Err(format!("expected an error, got {set:?}").into()),
        Err(e) => Ok(e),
    }
}

#[test]
fn registration_declares_the_gitlab_preset() {
    assert_eq!(REGISTRATION.kind, HostKind::GitLab);
    let [preset] = REGISTRATION.presets else {
        panic!("expected one preset: {:?}", REGISTRATION.presets);
    };
    assert_eq!(
        (preset.host, preset.api_base, preset.token_env),
        (GITLAB_HOST, GITLAB_API_BASE, "GITLAB_TOKEN")
    );
    assert_eq!(REGISTRATION.default_token_env, Some("GITLAB_TOKEN"));
}

#[test]
fn registration_infers_the_api_root_of_other_hosts() {
    assert_eq!((REGISTRATION.infer_api_base)(GITLAB_HOST), None);
    assert_eq!(
        (REGISTRATION.infer_api_base)("gitlab.example.com").as_deref(),
        Some("https://gitlab.example.com/api/v4")
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
        host: "gitlab.example.com".into(),
        kind: HostKind::GitLab,
        api_base: "https://gitlab.example.com/api/v4".into(),
        token_env: Some("_ACTUP_TEST_UNSET_GITLAB_TOKEN".into()),
        api_mode: ApiMode::Rest,
        max_tags_per_repo: NonZeroU8::MIN,
    })?;
    assert_eq!(provider.host(), "gitlab.example.com");
    assert_eq!(provider.kind(), actup_core::provider::Kind::GitLab);
    Ok(())
}

#[tokio::test]
async fn get_versions_reads_tags_and_the_default_branch() -> TestResult {
    let server = MockServer::start().await;
    serve_tags(
        &server,
        200,
        &format!("[{}, {}]", tag("v1.2.0", COMMIT_B), tag("v1.1.0", COMMIT_A)),
    )
    .await;
    serve_project(&server).await;

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

#[tokio::test]
async fn get_versions_follows_the_next_page_header() -> TestResult {
    let server = MockServer::start().await;
    let tags = format!("{PROJECT}/repository/tags");
    Mock::given(method("GET"))
        .and(path(&tags))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", "2")
                .set_body_string(format!("[{}]", tag("v2.0.0", COMMIT_B))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(&tags))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("[{}]", tag("v1.0.0", COMMIT_A))),
        )
        .mount(&server)
        .await;
    serve_project(&server).await;

    let set = provider(&server)?.get_versions(&repo()).await?;

    let names: Vec<&str> = set
        .semver_tags()
        .iter()
        .map(|t| t.version().as_str())
        .collect();
    assert_eq!(names, ["v2.0.0", "v1.0.0"]);
    Ok(())
}

async fn serve_tag_page(server: &MockServer, page: u32, per_page: &str, names: &[&str]) {
    let body: Vec<String> = names.iter().map(|name| tag(name, COMMIT_A)).collect();
    Mock::given(method("GET"))
        .and(path(format!("{PROJECT}/repository/tags")))
        .and(query_param("page", page.to_string()))
        .and(query_param("per_page", per_page))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", (page + 1).to_string())
                .set_body_string(format!("[{}]", body.join(","))),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn tag_names(set: &actup_core::version::Set) -> Vec<&str> {
    set.semver_tags()
        .iter()
        .map(|t| t.version().as_str())
        .collect()
}

#[tokio::test]
async fn get_versions_stops_at_max_tags() -> TestResult {
    let server = MockServer::start().await;
    serve_tag_page(&server, 1, "3", &["v4.0.0", "v3.0.0"]).await;
    serve_tag_page(&server, 2, "3", &["v2.0.0", "v1.0.0"]).await;
    serve_project(&server).await;

    let max_tags = NonZeroU8::new(3).ok_or("zero")?;
    let set = provider(&server)?
        .max_tags(max_tags)
        .get_versions(&repo())
        .await?;

    assert_eq!(tag_names(&set), ["v4.0.0", "v3.0.0", "v2.0.0"]);
    Ok(())
}

#[tokio::test]
async fn registration_passes_max_tags_per_repo() -> TestResult {
    let server = MockServer::start().await;
    serve_tag_page(&server, 1, "1", &["v2.0.0", "v1.0.0"]).await;
    serve_project(&server).await;
    let provider = (REGISTRATION.create)(ProviderConfig {
        host: "gitlab.com".into(),
        kind: HostKind::GitLab,
        api_base: format!("{}/api/v4", server.uri()),
        token_env: Some("_ACTUP_TEST_UNSET_GITLAB_TOKEN".into()),
        api_mode: ApiMode::Rest,
        max_tags_per_repo: NonZeroU8::MIN,
    })?;

    let set = provider.get_versions(&repo()).await?;

    assert_eq!(tag_names(&set), ["v2.0.0"]);
    Ok(())
}

#[tokio::test]
async fn get_versions_of_a_missing_project_is_not_found() -> TestResult {
    let error = versions_error(404, r#"{"message": "404 Project Not Found"}"#).await?;
    assert!(
        matches!(&error, Error::NotFound { owner, repo, host: Some(host) }
            if owner == "o" && repo == "r" && host == "gitlab.com"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_is_an_auth_error_with_the_message() -> TestResult {
    let error = versions_error(401, r#"{"message": "401 Unauthorized"}"#).await?;
    assert!(
        matches!(&error, Error::Auth { provider, reason, .. }
            if provider == "gitlab" && reason == "401 Unauthorized"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn forbidden_is_an_auth_error_with_the_oauth_description() -> TestResult {
    let error = versions_error(
        403,
        r#"{"error": "insufficient_scope", "error_description": "needs read_api"}"#,
    )
    .await?;
    assert!(
        matches!(&error, Error::Auth { reason, .. } if reason == "insufficient_scope"),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn too_many_requests_is_rate_limited_with_retry_after() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{PROJECT}/repository/tags")))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;

    let error = provider(&server)?
        .get_versions(&repo())
        .await
        .err()
        .ok_or("expected an error")?;

    assert!(
        matches!(&error, Error::RateLimited { provider, retry_after: Some(after) }
            if provider == "gitlab" && *after == Duration::from_secs(30)),
        "{error:?}"
    );
    assert!(error.is_environment_error());
    Ok(())
}

#[tokio::test]
async fn a_server_error_is_a_network_error() -> TestResult {
    let error = versions_error(502, "bad gateway").await?;
    assert!(
        matches!(&error, Error::Network { message, .. } if message.contains("502")),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_unparseable_body_is_an_internal_error() -> TestResult {
    let error = versions_error(200, "not json").await?;
    assert!(matches!(error, Error::Internal { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn resolve_a_branch_to_its_head() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{PROJECT}/repository/branches/main"),
        200,
        &branch(HEAD),
    )
    .await;

    let sha = provider(&server)?
        .resolve_ref_to_sha(&repo(), "main")
        .await?;

    assert_eq!(sha.as_str(), HEAD);
    Ok(())
}

#[tokio::test]
async fn resolve_an_annotated_tag_to_its_commit() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{PROJECT}/repository/branches/v1.2.0"),
        404,
        r#"{"message": "404 Branch Not Found"}"#,
    )
    .await;
    get(
        &server,
        &format!("{PROJECT}/repository/tags/v1.2.0"),
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
async fn resolve_stops_at_an_auth_error() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{PROJECT}/repository/branches/v1"),
        401,
        r#"{"message": "401 Unauthorized"}"#,
    )
    .await;

    let error = provider(&server)?
        .resolve_ref_to_sha(&repo(), "v1")
        .await
        .err()
        .ok_or("expected an error")?;

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
        &format!("{PROJECT}/repository/branches/main"),
        200,
        r#"{"name": "main", "commit": {"id": "abc123", "short_id": "abc123", "title": "t"}, "default": true}"#,
    )
    .await;

    let error = provider(&server)?
        .resolve_ref_to_sha(&repo(), "main")
        .await
        .err()
        .ok_or("expected an error")?;

    assert!(matches!(error, Error::Internal { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn get_default_branch_reads_the_project() -> TestResult {
    let server = MockServer::start().await;
    get(&server, PROJECT, 200, &project("trunk")).await;

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
        .and(path(format!("{PROJECT}/repository/files/README.md")))
        .and(query_param("ref", "v1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(file("# Hello\n")))
        .mount(&server)
        .await;

    let readme = provider(&server)?.get_readme(&repo(), Some("v1")).await?;

    assert_eq!(readme.as_deref(), Some("# Hello\n"));
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
        &format!("{PROJECT}/repository/files/action.yml"),
        200,
        &file("name: Hello\ndescription: Says hello\n"),
    )
    .await;

    let action = provider(&server)?.get_action_yml(&repo(), "v1").await?;

    assert_eq!(action.name(), Some("Hello"));
    assert_eq!(action.description(), Some("Says hello"));
    Ok(())
}

#[tokio::test]
async fn get_action_yml_falls_back_to_action_yaml_in_the_subpath() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{PROJECT}/repository/files/sub%2Faction.yml"),
        404,
        r#"{"message": "404 File Not Found"}"#,
    )
    .await;
    get(
        &server,
        &format!("{PROJECT}/repository/files/sub%2Faction.yaml"),
        200,
        &file("name: Nested\n"),
    )
    .await;

    let repo = ActionRepoId::new(None, "o", "r", Some("sub".to_string()));
    let action = provider(&server)?.get_action_yml(&repo, "v1").await?;

    assert_eq!(action.name(), Some("Nested"));
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
async fn get_action_yml_stops_at_a_rate_limit() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        &format!("{PROJECT}/repository/files/action.yml"),
        429,
        "",
    )
    .await;

    let error = provider(&server)?
        .get_action_yml(&repo(), "v1")
        .await
        .err()
        .ok_or("expected an error")?;

    assert!(matches!(error, Error::RateLimited { .. }), "{error:?}");
    Ok(())
}

#[tokio::test]
async fn a_token_is_sent_as_the_private_token_header() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PROJECT))
        .and(header("PRIVATE-TOKEN", "glpat-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string(project("main")))
        .mount(&server)
        .await;

    let provider = GitLabProvider::with_host(
        &format!("{}/api/v4", server.uri()),
        "gitlab.com",
        Some("glpat-test"),
    )?;

    assert_eq!(provider.get_default_branch(&repo()).await?, "main");
    Ok(())
}

#[tokio::test]
async fn the_api_base_is_the_api_root_as_given() -> TestResult {
    let server = MockServer::start().await;
    get(
        &server,
        "/gitlab/api/v4/projects/o%2Fr",
        200,
        &project("main"),
    )
    .await;

    let provider =
        GitLabProvider::with_host(&format!("{}/gitlab/api/v4/", server.uri()), "gl", None)?;

    assert_eq!(provider.get_default_branch(&repo()).await?, "main");
    Ok(())
}
