//! GitHub provider implementation

use actup_core::{
    action_ref::ActionRepoId,
    compat::BoxFuture,
    error::Result,
    provider::{self, ActionYml, Provider, Sha40},
    version,
};
use actup_providers::{RepoLabel, filename_label, list_refs, parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use tracing::{debug, warn};

use crate::client::GitHubClient;

/// GitHub provider for resolving action versions
pub struct GitHubProvider {
    client: GitHubClient,
    host: String,
    max_tags: u8,
    token: Option<String>,
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
            token: token.map(String::from),
        })
    }

    fn clone_url(&self, repo: &ActionRepoId) -> String {
        format!("https://{}/{}/{}.git", self.host, repo.owner(), repo.repo())
    }

    /// Fetch every tag and the default branch over the git wire protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if the remote rejects the connection or the repository
    /// does not exist.
    pub async fn versions_via_git(&self, repo: &ActionRepoId) -> Result<version::Set> {
        let label = RepoLabel::new(repo.owner(), repo.repo(), Some(self.host.clone()));
        let refs = list_refs(self.clone_url(repo), self.token.clone(), label).await?;

        debug!(
            owner = %repo.owner(),
            repo = %repo.repo(),
            tags = refs.tags().len(),
            "listed refs over git protocol"
        );

        Ok(refs.into_version_set())
    }

    /// Fetch the newest `max_tags` tags and the default branch over the REST API.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the REST calls fail.
    pub async fn versions_via_rest(&self, repo: &ActionRepoId) -> Result<version::Set> {
        let owner = repo.owner();
        let repo_name = repo.repo();

        debug!(owner = %owner, repo = %repo_name, "fetching versions from GitHub REST");

        let tags = self
            .client
            .list_tags_limited(owner, repo_name, self.max_tags)
            .await?;

        let mut version_set = process_tags(tags);

        let repo_info = self.client.get_repo(owner, repo_name).await?;
        let branch = self
            .client
            .get_branch(owner, repo_name, &repo_info.default_branch)
            .await?;

        version_set.set_default_branch(version::BranchInfo::new(
            repo_info.default_branch,
            branch.commit.sha,
        ));

        Ok(version_set)
    }

    #[must_use]
    pub(crate) fn graphql_endpoint(&self) -> String {
        if self.host == crate::GITHUB_HOST {
            return crate::GITHUB_GRAPHQL_URL.to_string();
        }

        if let Some(api_root) = self.client.api_base().strip_suffix("/api/v3") {
            return format!("{api_root}/api/graphql");
        }

        format!(
            "{}/api/graphql",
            self.client.api_base().trim_end_matches('/')
        )
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
        let repo = repo.clone();

        Box::pin(async move {
            match self.versions_via_git(&repo).await {
                Ok(versions) => Ok(versions),
                Err(e) if e.is_environment_error() => {
                    warn!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        error = %e,
                        "git protocol unavailable, falling back to REST"
                    );
                    self.versions_via_rest(&repo).await
                }
                Err(e) => Err(e),
            }
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
                match self.client.get_branch(&owner, &repo_name, &ref_name).await {
                    Ok(branch) => branch.commit.sha,
                    Err(actup_core::Error::NotFound { .. })
                        if actup_core::version::is_short_sha(&ref_name) =>
                    {
                        self.client
                            .get_commit(&owner, &repo_name, &ref_name)
                            .await?
                            .sha
                    }
                    Err(e) => return Err(e),
                }
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

            // Try action.yml first, then action.yaml (both are valid).
            // Track `loaded_path` so the parse-error label reflects what
            // actually succeeded, not the first path attempted.
            let (content, loaded_path) = match self
                .client
                .get_content(&owner, &repo_name, &action_path, Some(&ref_name))
                .await
            {
                Ok(content) => (content, action_path.clone()),
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
                        let c = self
                            .client
                            .get_content(&owner, &repo_name, &yaml_path, Some(&ref_name))
                            .await?;
                        (c, yaml_path)
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

            // Use the path that actually succeeded — parse errors otherwise
            // lie about which file caused them when the .yaml fallback fired.
            parse_action_yml(&raw, filename_label(&loaded_path))
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
        let result = process_tags(Vec::<GitHubTag>::new());
        assert_eq!(result.semver_tags(), []);
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

    #[test]
    fn test_graphql_endpoint_ghe_nonstandard() {
        let provider =
            GitHubProvider::with_host("https://custom.corp.com/github", "custom.corp.com", None)
                .unwrap();

        assert_eq!(
            provider.graphql_endpoint(),
            "https://custom.corp.com/github/api/graphql"
        );
    }
}
