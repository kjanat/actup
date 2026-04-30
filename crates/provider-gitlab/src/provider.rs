//! GitLab provider implementation

use crate::client::GitLabClient;
use actup_core::action_ref::ActionRepoId;
use actup_core::error::Result;
use actup_core::provider::{self, ActionYml, Provider, Sha40};
use actup_core::version;
use actup_providers::{parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::future::Future;
use std::pin::Pin;
use tracing::debug;

/// GitLab provider for resolving action versions
pub struct GitLabProvider {
    client: GitLabClient,
    host: String,
}

impl GitLabProvider {
    /// Create a new GitLab provider for gitlab.com
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        let client = GitLabClient::new(token)?;
        Ok(Self {
            host: crate::GITLAB_HOST.to_string(),
            client,
        })
    }

    /// Create a GitLab provider for a self-hosted instance
    ///
    /// # Arguments
    ///
    /// * `api_base` - API base URL (e.g., "<https://gitlab.example.com>")
    /// * `host` - Display host name (e.g., "gitlab.example.com")
    /// * `token` - Optional personal access token
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_host(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let client = GitLabClient::with_api_base(api_base, host, token)?;
        Ok(Self {
            host: host.to_string(),
            client,
        })
    }
}

impl Provider for GitLabProvider {
    fn kind(&self) -> provider::Kind {
        provider::Kind::GitLab
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<version::Set>> + Send + '_>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching versions from GitLab");

            // Get tags
            let tags = self.client.list_tags(&owner, &repo_name, None).await?;

            // Process into [`version::Set`]
            let mut version_set = process_tags(tags);

            // Get default branch info
            let project = self.client.get_project(&owner, &repo_name).await?;
            let branch = self
                .client
                .get_branch(&owner, &repo_name, &project.default_branch)
                .await?;

            version_set.set_default_branch(version::BranchInfo::new(
                project.default_branch,
                branch.commit.id,
            ));

            Ok(version_set)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, ref_name = %ref_name, "resolving ref to SHA");

            let commit_id = self
                .client
                .resolve_ref(&owner, &repo_name, &ref_name)
                .await?;

            Sha40::new(&commit_id).ok_or_else(|| actup_core::Error::Internal {
                context: format!("commit SHA from GitLab: {commit_id}"),
                message: "expected 40-character hex string".to_string(),
            })
        })
    }

    fn get_default_branch(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching default branch");

            let project = self.client.get_project(&owner, &repo_name).await?;
            Ok(project.default_branch)
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
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
                    .get_file(&owner, &repo_name, filename, &ref_name)
                    .await
                {
                    Ok(file) if file.encoding == "base64" => {
                        if let Ok(decoded) = STANDARD.decode(&file.content)
                            && let Ok(text) = String::from_utf8(decoded)
                        {
                            return Ok(Some(text));
                        }
                    }
                    _ => {}
                }
            }

            Ok(None)
        })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
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
                    .get_file(&owner, &repo_name, filename, &ref_name)
                    .await
                {
                    Ok(file) if file.encoding == "base64" => {
                        if let Ok(decoded) = STANDARD.decode(&file.content)
                            && let Ok(text) = String::from_utf8(decoded)
                        {
                            return parse_action_yml(&text);
                        }
                    }
                    // Only continue searching on NotFound; propagate auth/rate-limit/network errors
                    Err(actup_core::Error::NotFound { .. }) if filename == action_path.as_str() => {
                        debug!(
                            owner = %owner,
                            repo = %repo_name,
                            ref_name = %ref_name,
                            path = %filename,
                            "action.yml not found, trying action.yaml"
                        );
                    }
                    Err(actup_core::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e),
                    _ => {}
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
