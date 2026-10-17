//! GitHub provider tests against a mocked git smart-HTTP server.

use std::{collections::HashMap, num::NonZeroU8, sync::Arc};

use actup_core::{
    ActionRepoId, ApiMode, Config, Error, HostConfig, HostKind, Provider, Retention, cache,
    config::registration::ProviderConfig, fetch::Silent, provider, version,
};
use actup_provider_github::{GitHubProvider, REGISTRATION, create_batch_fetcher};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const HOST: &str = "ghe.test";
const TOKEN_ENV: &str = "_ACTUP_TEST_UNSET_GHE_TOKEN";
const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TAG_OBJECT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn pkt(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}

fn repo() -> ActionRepoId {
    ActionRepoId::new(Some(HOST.into()), "o", "r", None)
}

fn api_base(server: &MockServer) -> String {
    format!("{}/api/v3", server.uri())
}

async fn serve_refs(server: &MockServer) {
    let advertisement = [
        pkt("version 2\n"),
        pkt("agent=git/2.45.0\n"),
        pkt("ls-refs=unborn\n"),
        pkt("fetch=shallow\n"),
        pkt("object-format=sha1\n"),
        "0000".to_string(),
    ]
    .concat();
    Mock::given(method("GET"))
        .and(path("/o/r.git/info/refs"))
        .and(query_param("service", "git-upload-pack"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(advertisement, "application/x-git-upload-pack-advertisement"),
        )
        .mount(server)
        .await;

    let refs = [
        pkt(&format!("{HEAD} HEAD symref-target:refs/heads/main\n")),
        pkt(&format!("{HEAD} refs/heads/main\n")),
        pkt(&format!("{COMMIT_A} refs/tags/v1.0.0\n")),
        pkt(&format!(
            "{TAG_OBJECT} refs/tags/v1.1.0 peeled:{COMMIT_B}\n"
        )),
        "0000".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/o/r.git/git-upload-pack"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(refs, "application/x-git-upload-pack-result"),
        )
        .mount(server)
        .await;
}

async fn serve_missing_repo(server: &MockServer, lookups: u64) {
    Mock::given(method("GET"))
        .and(path("/o/r.git/info/refs"))
        .respond_with(ResponseTemplate::new(404))
        .expect(lookups)
        .mount(server)
        .await;
}

async fn refuse_rest(server: &MockServer, lookups: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/o/r/tags"))
        .respond_with(ResponseTemplate::new(404))
        .expect(lookups)
        .mount(server)
        .await;
}

fn assert_listed(set: &version::Set) {
    let latest = set.latest().map(|t| (t.version().as_str(), t.sha()));
    assert_eq!(latest, Some(("v1.1.0", COMMIT_B)));
    assert_eq!(set.tag_sha("v1.0.0"), Some(COMMIT_A));
    assert_eq!(
        set.default_branch().map(|b| (b.name(), b.sha())),
        Some(("main", HEAD))
    );
    assert_eq!(set.listing(), version::Listing::Complete);
}

fn provider_config(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        host: HOST.into(),
        kind: HostKind::GitHub,
        api_base: api_base(server),
        token_env: Some(TOKEN_ENV.into()),
        api_mode: ApiMode::Rest,
        max_tags_per_repo: NonZeroU8::MIN,
    }
}

#[tokio::test]
async fn get_versions_lists_refs_over_git() -> TestResult {
    let server = MockServer::start().await;
    serve_refs(&server).await;

    let provider = GitHubProvider::with_host(&api_base(&server), HOST, None)?;
    let set = provider.get_versions(&repo()).await?;

    assert_listed(&set);
    Ok(())
}

#[tokio::test]
async fn get_versions_of_a_repo_git_does_not_find_is_not_found() -> TestResult {
    let server = MockServer::start().await;
    serve_missing_repo(&server, 1).await;
    refuse_rest(&server, 0).await;

    let provider = GitHubProvider::with_host(&api_base(&server), HOST, None)?;
    let error = provider.get_versions(&repo()).await.err().ok_or("listed")?;

    assert!(
        matches!(&error, Error::NotFound { owner, repo, host: Some(host) }
            if owner == "o" && repo == "r" && host == HOST),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test]
async fn batch_fetcher_lists_refs_over_git() -> TestResult {
    let server = MockServer::start().await;
    serve_refs(&server).await;

    let fetcher = create_batch_fetcher(&provider_config(&server), Arc::new(cache::Noop))?;
    let listed = fetcher.fetch_versions(&[repo()], &Silent).await;

    let set = listed.get(&repo()).ok_or("not listed")?.as_ref();
    assert_listed(set.map_err(ToString::to_string)?);
    Ok(())
}

#[tokio::test]
async fn batch_fetcher_reports_a_repo_git_does_not_find() -> TestResult {
    let server = MockServer::start().await;
    serve_missing_repo(&server, 1).await;
    refuse_rest(&server, 0).await;

    let fetcher = create_batch_fetcher(&provider_config(&server), Arc::new(cache::Noop))?;
    let listed = fetcher.fetch_versions(&[repo()], &Silent).await;

    let result = listed.get(&repo()).ok_or("no result")?;
    assert!(
        matches!(result, Err(Error::NotFound { host: Some(host), .. }) if host == HOST),
        "{result:?}"
    );
    Ok(())
}

fn registry_config(server: &MockServer) -> Config {
    Config {
        hosts: HashMap::from([(
            HOST.to_string(),
            HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: Some(api_base(server)),
                token_env: Some(TOKEN_ENV.into()),
                api_mode: Some(ApiMode::Rest),
                max_tags_per_repo: None,
            },
        )]),
        ..Config::default()
    }
}

#[tokio::test]
async fn registry_fetch_lists_refs_over_git() -> TestResult {
    let server = MockServer::start().await;
    serve_refs(&server).await;
    let config = registry_config(&server);
    let registry = provider::Registry::from_config(&config, &[&REGISTRATION], &Retention::Run)?;

    let results = actup_provider_github::fetch_versions_with_registry(
        &[repo()],
        &config,
        &registry,
        Arc::new(cache::Noop),
        &Silent,
    )
    .await;

    let set = results.get(&repo()).ok_or("not fetched")?.as_ref();
    assert_listed(set.map_err(ToString::to_string)?);
    Ok(())
}

#[tokio::test]
async fn registry_fetch_reports_a_repo_git_does_not_find() -> TestResult {
    let server = MockServer::start().await;
    serve_missing_repo(&server, 1).await;
    refuse_rest(&server, 0).await;
    let config = registry_config(&server);
    let registry = provider::Registry::from_config(&config, &[&REGISTRATION], &Retention::Run)?;

    let results = actup_provider_github::fetch_versions_with_registry(
        &[repo()],
        &config,
        &registry,
        Arc::new(cache::Noop),
        &Silent,
    )
    .await;

    let result = results.get(&repo()).ok_or("no result")?;
    assert!(
        matches!(result, Err(Error::NotFound { host: Some(host), .. }) if host == HOST),
        "{result:?}"
    );
    Ok(())
}
