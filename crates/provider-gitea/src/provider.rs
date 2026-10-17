//! Gitea provider implementation

use std::num::NonZeroU8;

use actup_core::{
    action_ref::ActionRepoId,
    compat::BoxFuture,
    config::DEFAULT_MAX_TAGS_PER_REPO,
    error::Result,
    provider::{self, ActionYml, Provider, Sha40},
    version,
};
use actup_providers::{filename_label, parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use tracing::debug;

use crate::{client::GiteaClient, types::GiteaContent};

/// Gitea provider for resolving action versions
///
/// Also compatible with Forgejo, which is a soft fork of Gitea with the same API.
pub struct GiteaProvider {
    client: GiteaClient,
    host: String,
    max_tags: NonZeroU8,
}

impl GiteaProvider {
    /// Create a new Gitea provider for a specific instance
    ///
    /// # Arguments
    ///
    /// * `api_base` - API root URL (e.g., "<https://codeberg.org/api/v1>")
    /// * `host` - Display host name (e.g., "gitea.com")
    /// * `token` - Optional API token
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created.
    pub fn new(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let client = GiteaClient::new(api_base, host, token)?;
        Ok(Self {
            host: host.to_string(),
            client,
            max_tags: DEFAULT_MAX_TAGS_PER_REPO,
        })
    }

    /// Fetch at most `max_tags` tags per repository.
    #[must_use]
    pub const fn max_tags(mut self, max_tags: NonZeroU8) -> Self {
        self.max_tags = max_tags;
        self
    }
}

fn decode_base64_file_text(content: &GiteaContent) -> Option<String> {
    if content.content_type != "file" {
        return None;
    }

    let encoded = content.content.as_deref()?;
    if content.encoding.as_deref()? != "base64" {
        return None;
    }

    let decoded = STANDARD.decode(encoded).ok()?;
    String::from_utf8(decoded).ok()
}

impl Provider for GiteaProvider {
    fn kind(&self) -> provider::Kind {
        provider::Kind::Gitea
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching versions from Gitea");

            // Get tags
            let tags = self
                .client
                .list_tags(&owner, &repo_name, self.max_tags)
                .await?;

            // Process into [`version::Set`]
            let mut version_set = process_tags(tags);

            // Get default branch info
            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            let branch = self
                .client
                .get_branch(&owner, &repo_name, &repo_info.default_branch)
                .await?;

            version_set.set_default_branch(version::BranchInfo::new(
                repo_info.default_branch,
                branch.commit.id,
            ));

            Ok(version_set)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, ref_name = %ref_name, "resolving ref to SHA");

            let sha = self
                .client
                .resolve_ref(&owner, &repo_name, &ref_name)
                .await?;

            Sha40::new(&sha).ok_or_else(|| actup_core::Error::Internal {
                context: format!("commit SHA from Gitea: {sha}"),
                message: "expected 40-character hex string".to_string(),
                source: None,
            })
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching default branch");

            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            Ok(repo_info.default_branch)
        })
    }

    fn get_repo_state(
        &self,
        repo: &ActionRepoId,
    ) -> BoxFuture<'_, Result<Option<provider::RepoState>>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            let info = self.client.get_repo_archive(&owner, &repo_name).await?;
            Ok(Some(if info.archived {
                provider::RepoState::Archived
            } else {
                provider::RepoState::Active
            }))
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.unwrap_or("HEAD").to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, ref_name = %ref_name, "fetching README");

            // Try common README filenames
            for filename in [
                "README.md",
                "README.MD",
                "readme.md",
                "README",
                "README.txt",
            ] {
                match self
                    .client
                    .get_contents(&owner, &repo_name, filename, &ref_name)
                    .await
                {
                    Ok(content) => {
                        if let Some(text) = decode_base64_file_text(&content) {
                            return Ok(Some(text));
                        }
                    }
                    // NotFound is expected - try next filename
                    Err(actup_core::Error::NotFound { .. }) => {}
                    // Propagate auth/rate-limit/network errors
                    Err(e) => return Err(e),
                }
            }

            Ok(None)
        })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();
        let subpath = repo.subpath().map(std::string::ToString::to_string);

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, ref_name = %ref_name, "fetching action.yml");

            // Determine action.yml path
            let action_path = subpath.map_or_else(
                || "action.yml".to_string(),
                |sub| format!("{sub}/action.yml"),
            );

            // Try action.yml, then action.yaml
            for filename in [action_path.as_str(), &action_path.replace(".yml", ".yaml")] {
                match self
                    .client
                    .get_contents(&owner, &repo_name, filename, &ref_name)
                    .await
                {
                    Ok(content) => {
                        if let Some(text) = decode_base64_file_text(&content) {
                            return parse_action_yml(&text, filename_label(filename));
                        }
                    }
                    // Only continue searching on NotFound; propagate auth/rate-limit/network errors
                    Err(actup_core::Error::NotFound { .. }) => {
                        if filename == action_path.as_str() {
                            debug!(
                                owner = %owner,
                                repo = %repo_name,
                                ref_name = %ref_name,
                                path = %filename,
                                "action.yml not found, trying action.yaml"
                            );
                        }
                    }
                    Err(e) => return Err(e),
                }
            }

            // Neither action.yml nor action.yaml was found
            Err(actup_core::Error::NotFound {
                owner: owner.clone(),
                repo: repo_name.clone(),
                host: Some(self.client.host().to_string()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    const FULL_SHA: &str = "abc1234def5678abc1234def5678abc1234def56";

    async fn respond(server: &MockServer, url: String, status: u16, body: String) {
        Mock::given(method("GET"))
            .and(path(url))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    async fn resolve(ref_name: &str) -> (MockServer, Result<Sha40>) {
        let server = MockServer::start().await;
        for missing in MISSING {
            respond(
                &server,
                format!("{BASE}/{}", missing.replace("REF", ref_name)),
                404,
                r#"{"message": "not found"}"#.to_string(),
            )
            .await;
        }
        respond(
            &server,
            format!("{BASE}/git/commits/abc1234"),
            200,
            commit_body(),
        )
        .await;
        let api_base = format!("{}/api/v1", server.uri());
        let result = match GiteaProvider::new(&api_base, "gitea.com", None) {
            Ok(provider) => {
                provider
                    .resolve_ref_to_sha(&ActionRepoId::simple("o", "r"), ref_name)
                    .await
            }
            Err(e) => Err(e),
        };
        (server, result)
    }

    #[tokio::test]
    async fn resolve_short_sha_falls_back_to_commit()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (_server, result) = resolve("abc1234").await;
        assert_eq!(result?.as_str(), FULL_SHA);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_missing_name_skips_commit_lookup() {
        let (server, result) = resolve("nope").await;
        assert!(
            matches!(result, Err(actup_core::Error::RefNotFound { .. })),
            "{result:?}"
        );
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests.iter().all(|r| !r.url.path().contains("/commits/")));
    }

    #[tokio::test]
    async fn repo_state_reads_archived() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        for (repo, archived) in [("live", false), ("old", true)] {
            respond(
                &server,
                format!("/api/v1/repos/o/{repo}"),
                200,
                format!(
                    r#"{{"id": 1, "name": "{repo}", "full_name": "o/{repo}", "description": null, "empty": false, "private": false, "fork": false, "default_branch": "main", "clone_url": "c", "ssh_url": "s", "html_url": "h", "owner": {{"id": 1, "login": "o", "full_name": null, "email": null, "avatar_url": "a"}}, "archived": {archived}}}"#
                ),
            )
            .await;
        }
        let provider = GiteaProvider::new(&format!("{}/api/v1", server.uri()), "gitea.com", None)?;
        let state = |repo: &str| provider.get_repo_state(&ActionRepoId::simple("o", repo));
        assert_eq!(state("live").await?, Some(provider::RepoState::Active));
        assert_eq!(state("old").await?, Some(provider::RepoState::Archived));
        Ok(())
    }

    const BASE: &str = "/api/v1/repos/o/r";
    const MISSING: [&str; 2] = ["tags/REF", "branches/REF"];

    fn commit_body() -> String {
        let author = r#"{"name": "a", "email": "a@example.com", "date": "2026-01-01T00:00:00Z"}"#;
        format!(
            r#"{{"sha": "{FULL_SHA}", "url": "u", "html_url": "h", "commit": {{"message": "m", "author": {author}, "committer": {author}}}}}"#
        )
    }
}
