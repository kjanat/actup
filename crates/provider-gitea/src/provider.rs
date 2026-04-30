//! Gitea provider implementation

use crate::client::GiteaClient;
use crate::types::GiteaContent;
use actup_core::action_ref::ActionRepoId;
use actup_core::compat::BoxFuture;
use actup_core::error::Result;
use actup_core::provider::{self, ActionYml, Provider, Sha40};
use actup_core::version;
use actup_providers::{filename_label, parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use tracing::debug;

/// Gitea provider for resolving action versions
///
/// Also compatible with Forgejo, which is a soft fork of Gitea with the same API.
pub struct GiteaProvider {
    client: GiteaClient,
    host: String,
}

impl GiteaProvider {
    /// Create a new Gitea provider for a specific instance
    ///
    /// # Arguments
    ///
    /// * `api_base` - API base URL (e.g., "<https://gitea.com>" or "<https://git.example.com>")
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
        })
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
            let tags = self.client.list_tags(&owner, &repo_name, None).await?;

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

            let git_ref = self
                .client
                .get_git_ref(&owner, &repo_name, &ref_name)
                .await?;

            Sha40::new(&git_ref.object.sha).ok_or_else(|| actup_core::Error::Internal {
                context: format!("commit SHA from Gitea: {}", git_ref.object.sha),
                message: "expected 40-character hex string".to_string(),
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
