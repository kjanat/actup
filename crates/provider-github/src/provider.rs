//! GitHub provider implementation

use crate::client::GitHubClient;
use actup_core::action_ref::ActionRepoId;
use actup_core::compat::BoxFuture;
use actup_core::error::Result;
use actup_core::provider::{self, ActionYml, Provider, Sha40};
use actup_core::version;
use actup_providers::{parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use tracing::debug;

/// GitHub provider for resolving action versions
pub struct GitHubProvider {
    client: GitHubClient,
    host: String,
    max_tags: u8,
}

impl GitHubProvider {
    /// Create a new GitHub provider for github.com
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        Self::with_host_and_max_tags(
            crate::GITHUB_API_HOST,
            crate::GITHUB_HOST,
            token,
            crate::graphql::DEFAULT_MAX_TAGS_PER_REPO,
        )
    }

    /// Create a GitHub provider for a custom host (e.g., GitHub Enterprise)
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_host(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        Self::with_host_and_max_tags(
            api_base,
            host,
            token,
            crate::graphql::DEFAULT_MAX_TAGS_PER_REPO,
        )
    }

    /// Create a GitHub provider with an explicit tag fetch cap.
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_host_and_max_tags(
        api_base: &str,
        host: &str,
        token: Option<&str>,
        max_tags: u8,
    ) -> Result<Self> {
        let client = GitHubClient::with_api_base(api_base, host, token)?;
        Ok(Self {
            host: host.to_string(),
            client,
            max_tags: max_tags.max(1),
        })
    }

    #[must_use]
    pub(crate) fn graphql_endpoint(&self) -> String {
        if self.host == crate::GITHUB_HOST {
            return crate::GITHUB_GRAPHQL_URL.to_string();
        }

        if let Some(api_root) = self.client.api_base().strip_suffix("/api/v3") {
            return format!("{api_root}/api/graphql");
        }

        format!("{}/graphql", self.client.api_base().trim_end_matches('/'))
    }
}

impl Provider for GitHubProvider {
    fn kind(&self) -> provider::Kind {
        provider::Kind::GitHub
    }

    fn supports_batch_fetch(&self) -> bool {
        true
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching versions from GitHub");

            // Only fetch the newest configured tags. Older refs fall back to
            // direct resolution when needed instead of walking the full tag history.
            let tags = self
                .client
                .list_tags_limited(&owner, &repo_name, self.max_tags)
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
                branch.commit.sha,
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
            debug!(
                owner = %owner,
                repo = %repo_name,
                ref_name = %ref_name,
                "resolving ref to SHA"
            );

            // Try as a tag first
            let git_ref = self
                .client
                .get_ref(&owner, &repo_name, &format!("tags/{ref_name}"))
                .await;

            let sha = if let Ok(r) = git_ref {
                if r.object.object_type == "tag" {
                    // Annotated tag - need to dereference to get commit SHA
                    let tag_obj = self
                        .client
                        .get_tag_object(&owner, &repo_name, &r.object.sha)
                        .await?;
                    tag_obj.object.sha
                } else {
                    // Lightweight tag or commit - SHA is already the commit
                    r.object.sha
                }
            } else {
                // Try as a branch
                let branch = self
                    .client
                    .get_branch(&owner, &repo_name, &ref_name)
                    .await?;
                branch.commit.sha
            };

            Sha40::new(&sha).ok_or_else(|| actup_core::Error::Internal {
                context: "resolve_ref_to_sha".to_string(),
                message: format!("GitHub returned invalid SHA: {sha}"),
            })
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
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
        let ref_name = ref_name.map(String::from);

        Box::pin(async move {
            let result = self
                .client
                .get_content(&owner, &repo_name, "README.md", ref_name.as_deref())
                .await;

            match result {
                Ok(content) => {
                    if let (Some(encoded), Some("base64")) =
                        (content.content, content.encoding.as_deref())
                    {
                        // Decode base64 (GitHub returns content with newlines)
                        let cleaned = encoded.replace('\n', "");
                        let bytes =
                            STANDARD
                                .decode(&cleaned)
                                .map_err(|e| actup_core::Error::Internal {
                                    context: "base64 decode".into(),
                                    message: format!("Failed to decode README: {e}"),
                                })?;
                        let decoded =
                            String::from_utf8(bytes).map_err(|e| actup_core::Error::Internal {
                                context: "utf8 decode".into(),
                                message: format!("README is not valid UTF-8: {e}"),
                            })?;
                        Ok(Some(decoded))
                    } else {
                        Ok(None)
                    }
                }
                Err(actup_core::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
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
        let subpath = repo.subpath().map(str::to_owned);

        Box::pin(async move {
            // Determine base path for action file
            let action_path = match &subpath {
                Some(p) if p.ends_with("action.yml") || p.ends_with("action.yaml") => p.clone(),
                Some(p) => format!("{p}/action.yml"),
                None => "action.yml".to_string(),
            };

            // Try action.yml first, then action.yaml (both are valid)
            let content = match self
                .client
                .get_content(&owner, &repo_name, &action_path, Some(&ref_name))
                .await
            {
                Ok(content) => content,
                Err(actup_core::Error::NotFound { .. }) => {
                    if std::path::Path::new(&action_path)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("yml"))
                    {
                        let yaml_path = std::path::Path::new(&action_path)
                            .with_extension("yaml")
                            .to_string_lossy()
                            .to_string();
                        debug!(
                            owner = %owner,
                            repo = %repo_name,
                            "action.yml not found, trying action.yaml"
                        );
                        self.client
                            .get_content(&owner, &repo_name, &yaml_path, Some(&ref_name))
                            .await?
                    } else {
                        return Err(actup_core::Error::NotFound {
                            owner: owner.clone(),
                            repo: repo_name.clone(),
                            host: Some(self.host.clone()),
                        });
                    }
                }
                Err(e) => return Err(e),
            };

            // Decode content
            let raw = if let (Some(encoded), Some("base64")) =
                (content.content, content.encoding.as_deref())
            {
                // Remove newlines and decode
                let cleaned = encoded.replace('\n', "");
                let bytes = STANDARD
                    .decode(&cleaned)
                    .map_err(|e| actup_core::Error::Internal {
                        context: "base64 decode".into(),
                        message: format!("Failed to decode action.yml: {e}"),
                    })?;
                String::from_utf8(bytes).map_err(|e| actup_core::Error::Internal {
                    context: "utf8 decode".into(),
                    message: format!("action.yml is not valid UTF-8: {e}"),
                })?
            } else {
                return Err(actup_core::Error::Internal {
                    context: "get_action_yml".into(),
                    message: format!(
                        "GitHub returned content without base64 encoding for {owner}/{repo_name}"
                    ),
                });
            };

            // Parse the YAML content
            parse_action_yml(&raw)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::types::{GitHubTag, TagCommit};

    // Helper to create a GitHubTag for testing
    fn make_tag(name: &str, sha: &str) -> GitHubTag {
        GitHubTag {
            name: name.to_string(),
            commit: TagCommit {
                sha: sha.to_string(),
            },
        }
    }

    // process_tags tests

    #[test]
    fn test_process_tags_empty() {
        let result = process_tags(vec![] as Vec<GitHubTag>);
        assert!(result.semver_tags().is_empty());
        assert!(result.latest().is_none());
        assert!(result.latest_per_major().is_empty());
    }

    #[test]
    fn test_process_tags_single_version() {
        let tags = vec![make_tag("v1.0.0", "aaa111")];
        let result = process_tags(tags);

        assert_eq!(result.semver_tags().len(), 1);
        assert!(result.latest().is_some());
        assert_eq!(
            result.latest().as_ref().unwrap().version().to_string(),
            "v1.0.0"
        );
        assert_eq!(result.latest_per_major().len(), 1);
        assert!(result.latest_per_major().contains_key(&1));
    }

    #[test]
    fn test_process_tags_multiple_versions_sorted() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0", "bbb"),
            make_tag("v1.5.0", "ccc"),
        ];
        let result = process_tags(tags);

        // Should be sorted descending
        assert_eq!(result.semver_tags().len(), 3);
        assert_eq!(result.semver_tags()[0].version().to_string(), "v2.0.0");
        assert_eq!(result.semver_tags()[1].version().to_string(), "v1.5.0");
        assert_eq!(result.semver_tags()[2].version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_latest_tracking() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0", "bbb"),
            make_tag("v3.0.0", "ccc"),
        ];
        let result = process_tags(tags);

        assert!(result.latest().is_some());
        assert_eq!(result.latest().unwrap().version().to_string(), "v3.0.0");
    }

    #[test]
    fn test_process_tags_latest_per_major() {
        let tags = vec![
            make_tag("v1.0.0", "a"),
            make_tag("v1.1.0", "b"),
            make_tag("v1.2.0", "c"),
            make_tag("v2.0.0", "d"),
            make_tag("v2.1.0", "e"),
        ];
        let result = process_tags(tags);

        assert_eq!(result.latest_per_major().len(), 2);
        assert_eq!(
            result
                .latest_per_major()
                .get(&1)
                .unwrap()
                .version()
                .to_string(),
            "v1.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .unwrap()
                .version()
                .to_string(),
            "v2.1.0"
        );
    }

    #[test]
    fn test_process_tags_includes_prereleases_in_tags() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0-beta.1", "bbb"),
            make_tag("v2.0.0-rc.1", "ccc"),
        ];
        let result = process_tags(tags);

        // Prereleases are included in semver_tags (filtered at query time)
        assert_eq!(result.semver_tags().len(), 3);
        // But "latest" only tracks stable releases
        assert_eq!(result.latest().unwrap().version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_non_semver_ignored() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("latest", "bbb"),
            make_tag("main", "ccc"),
        ];
        let result = process_tags(tags);

        // Non-semver should be ignored
        assert_eq!(result.semver_tags().len(), 1);
        assert_eq!(result.latest().unwrap().version().to_string(), "v1.0.0");
    }

    #[test]
    fn test_graphql_endpoint_public_github() {
        let provider = GitHubProvider::new(None).unwrap();
        assert_eq!(provider.graphql_endpoint(), crate::GITHUB_GRAPHQL_URL);
    }

    #[test]
    fn test_graphql_endpoint_ghe() {
        let provider =
            GitHubProvider::with_host("https://ghe.corp.com/api/v3", "ghe.corp.com", None).unwrap();

        assert_eq!(
            provider.graphql_endpoint(),
            "https://ghe.corp.com/api/graphql"
        );
    }
}
