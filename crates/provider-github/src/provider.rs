//! GitHub provider implementation

use std::{num::NonZeroU8, sync::Arc};

use actup_core::{
    action_ref::ActionRepoId,
    compat::BoxFuture,
    config::DEFAULT_MAX_TAGS_PER_REPO,
    encode_segment,
    error::Result,
    provider::{self, ActionYml, Provider, RepoSummary, Search, Sha40},
    version,
};
use actup_providers::{RepoLabel, filename_label, list_refs, parse_action_yml, process_tags};
use base64::{Engine, engine::general_purpose::STANDARD};
use tracing::{debug, warn};

use crate::{
    client::{Conditional, GitHubClient, Tagged},
    types::{GitHubBranch, GitHubRepo, GitHubTag},
};

const TAGS_ETAG: &str = "github.tags";
const REPO_ETAG: &str = "github.repo";
const BRANCH_ETAG: &str = "github.branch";

/// The `ETag`s of the three REST calls behind a version set, and its default branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestValidators {
    tags: String,
    repo: String,
    branch: String,
    default_branch: String,
}

impl RestValidators {
    /// Read the validators a cache entry stored for `versions`, when all three `ETag`s are there.
    #[must_use]
    pub fn from_entry(
        validators: &std::collections::BTreeMap<String, String>,
        versions: &version::Set,
    ) -> Option<Self> {
        Some(Self {
            tags: validators.get(TAGS_ETAG)?.clone(),
            repo: validators.get(REPO_ETAG)?.clone(),
            branch: validators.get(BRANCH_ETAG)?.clone(),
            default_branch: versions.default_branch()?.name().to_string(),
        })
    }
}

/// The versions a REST listing returned, with the `ETag`s of its three calls keyed for a cache entry.
#[derive(Debug)]
pub struct RestListing {
    /// The listed versions.
    pub versions: version::Set,
    /// The `ETag`s the three calls returned.
    pub validators: std::collections::BTreeMap<String, String>,
}

impl RestListing {
    fn new(
        tags: Tagged<Vec<GitHubTag>>,
        repo: Tagged<GitHubRepo>,
        branch: Tagged<GitHubBranch>,
    ) -> Self {
        let mut versions = process_tags(tags.body);
        versions.set_default_branch(version::BranchInfo::new(
            repo.body.default_branch,
            branch.body.commit.sha,
        ));
        let validators = [
            (TAGS_ETAG, tags.etag),
            (REPO_ETAG, repo.etag),
            (BRANCH_ETAG, branch.etag),
        ]
        .into_iter()
        .filter_map(|(key, etag)| Some((key.to_string(), etag?)))
        .collect();
        Self {
            versions,
            validators,
        }
    }
}

/// The answer of the REST API to a revalidation with stored `ETag`s.
#[derive(Debug)]
pub enum Revalidation {
    /// All three calls answered 304.
    NotModified,
    /// At least one call sent a new body.
    Modified(Box<RestListing>),
}

/// GitHub provider for resolving action versions
pub struct GitHubProvider {
    client: GitHubClient,
    host: String,
    max_tags: NonZeroU8,
    token: Option<String>,
}

impl GitHubProvider {
    /// Create a new GitHub provider for github.com
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        Self::with_host(crate::GITHUB_API_HOST, crate::GITHUB_HOST, token)
    }

    /// Create a GitHub provider for a custom host (e.g., GitHub Enterprise)
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_host(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let client = GitHubClient::with_api_base(api_base, host, token)?;
        Ok(Self {
            host: host.to_string(),
            client,
            max_tags: DEFAULT_MAX_TAGS_PER_REPO,
            token: token.map(String::from),
        })
    }

    /// Fetch at most `max_tags` tags per repository over the API.
    #[must_use]
    pub const fn max_tags(mut self, max_tags: NonZeroU8) -> Self {
        self.max_tags = max_tags;
        self
    }

    fn git_base(&self) -> String {
        if self.host == crate::GITHUB_HOST {
            return format!("https://{}", crate::GITHUB_HOST);
        }
        self.client
            .api_base()
            .trim_end_matches('/')
            .strip_suffix("/api/v3")
            .map_or_else(|| format!("https://{}", self.host), str::to_string)
    }

    fn clone_url(&self, repo: &ActionRepoId) -> Result<String> {
        Ok(format!(
            "{}/{}/{}.git",
            self.git_base(),
            encode_segment(repo.owner())?,
            encode_segment(repo.repo())?
        ))
    }

    /// Fetch every tag and the default branch over the git wire protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if the remote rejects the connection or the repository
    /// does not exist.
    pub async fn versions_via_git(&self, repo: &ActionRepoId) -> Result<version::Set> {
        let label = RepoLabel::new(repo.owner(), repo.repo(), Some(self.host.clone()));
        let refs = list_refs(self.clone_url(repo)?, self.token.clone(), label).await?;

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
        Ok(self.list_via_rest(repo).await?.versions)
    }

    /// Fetch versions over the REST API with the `ETag` of each call.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the REST calls fail.
    pub async fn list_via_rest(&self, repo: &ActionRepoId) -> Result<RestListing> {
        let (owner, name) = (repo.owner(), repo.repo());
        debug!(owner = %owner, repo = %name, "fetching versions from GitHub REST");
        let tags = self
            .client
            .list_tags_limited_tagged(owner, name, self.max_tags)
            .await?;
        let repo_info = self.client.get_repo_tagged(owner, name).await?;
        let branch = self
            .client
            .get_branch_tagged(owner, name, &repo_info.body.default_branch)
            .await?;
        Ok(RestListing::new(tags, repo_info, branch))
    }

    /// Revalidate versions over the REST API, sending the `ETag`s of `since` as `If-None-Match`.
    ///
    /// Answers [`Revalidation::NotModified`] only when the tags' first page,
    /// the repository and its default branch all answer 304. Any other answer
    /// refetches each 304 call without a validator.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the REST calls fail.
    pub async fn revalidate_via_rest(
        &self,
        repo: &ActionRepoId,
        since: &RestValidators,
    ) -> Result<Revalidation> {
        let (owner, name) = (repo.owner(), repo.repo());
        debug!(owner = %owner, repo = %name, "revalidating versions over GitHub REST");
        let tags = self
            .client
            .list_tags_limited_if_none_match(owner, name, self.max_tags, &since.tags)
            .await?;
        let repo_info = self
            .client
            .get_repo_if_none_match(owner, name, &since.repo)
            .await?;
        let branch_name = match &repo_info {
            Conditional::Modified(tagged) => tagged.body.default_branch.clone(),
            Conditional::NotModified => since.default_branch.clone(),
        };
        let branch = if branch_name == since.default_branch {
            self.client
                .get_branch_if_none_match(owner, name, &branch_name, &since.branch)
                .await?
        } else {
            Conditional::Modified(
                self.client
                    .get_branch_tagged(owner, name, &branch_name)
                    .await?,
            )
        };

        if matches!(
            (&tags, &repo_info, &branch),
            (
                Conditional::NotModified,
                Conditional::NotModified,
                Conditional::NotModified
            )
        ) {
            debug!(owner = %owner, repo = %name, "versions not modified");
            return Ok(Revalidation::NotModified);
        }

        let tags = match tags {
            Conditional::Modified(tagged) => tagged,
            Conditional::NotModified => {
                self.client
                    .list_tags_limited_tagged(owner, name, self.max_tags)
                    .await?
            }
        };
        let repo_info = match repo_info {
            Conditional::Modified(tagged) => tagged,
            Conditional::NotModified => self.client.get_repo_tagged(owner, name).await?,
        };
        let branch = match branch {
            Conditional::Modified(tagged) => tagged,
            Conditional::NotModified => {
                self.client
                    .get_branch_tagged(owner, name, &branch_name)
                    .await?
            }
        };
        Ok(Revalidation::Modified(Box::new(RestListing::new(
            tags, repo_info, branch,
        ))))
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
                        error = %actup_core::error::chain(&e),
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
                source: None,
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
                                    message: "Failed to decode README".into(),
                                    source: Some(Arc::new(e)),
                                })?;
                        let decoded =
                            String::from_utf8(bytes).map_err(|e| actup_core::Error::Internal {
                                context: "utf8 decode".into(),
                                message: "README is not valid UTF-8".into(),
                                source: Some(Arc::new(e)),
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
                        message: "Failed to decode action.yml".into(),
                        source: Some(Arc::new(e)),
                    })?;
                String::from_utf8(bytes).map_err(|e| actup_core::Error::Internal {
                    context: "utf8 decode".into(),
                    message: "action.yml is not valid UTF-8".into(),
                    source: Some(Arc::new(e)),
                })?
            } else {
                return Err(actup_core::Error::Internal {
                    context: "get_action_yml".into(),
                    message: format!(
                        "GitHub returned content without base64 encoding for {owner}/{repo_name}"
                    ),
                    source: None,
                });
            };

            // Use the path that actually succeeded — parse errors otherwise
            // lie about which file caused them when the .yaml fallback fired.
            parse_action_yml(&raw, filename_label(&loaded_path))
        })
    }

    fn get_repo_state(
        &self,
        repo: &ActionRepoId,
    ) -> BoxFuture<'_, Result<Option<provider::RepoState>>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            let info = self.client.get_repo(&owner, &repo_name).await?;
            Ok(Some(if info.disabled {
                provider::RepoState::Disabled
            } else if info.archived {
                provider::RepoState::Archived
            } else {
                provider::RepoState::Active
            }))
        })
    }

    fn search_actions(&self, query: &str) -> BoxFuture<'_, Result<Search>> {
        let query = query.to_string();

        Box::pin(async move {
            let found = self.client.search_repositories(&query).await?;
            Ok(Search::Found(
                found
                    .items
                    .into_iter()
                    .map(|item| RepoSummary {
                        owner: item.owner.login,
                        repo: item.name,
                        description: item.description,
                    })
                    .collect(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
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
    fn test_process_tags_single_version() -> TestResult {
        let tags = vec![make_tag("v1.0.0", "aaa111")];
        let result = process_tags(tags);

        assert_eq!(result.semver_tags().len(), 1);
        assert!(result.latest().is_some());
        assert_eq!(
            result
                .latest()
                .as_ref()
                .ok_or("`result.latest().as_ref()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        assert_eq!(result.latest_per_major().len(), 1);
        assert!(result.latest_per_major().contains_key(&1));
        Ok(())
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
    fn test_process_tags_latest_tracking() -> TestResult {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0", "bbb"),
            make_tag("v3.0.0", "ccc"),
        ];
        let result = process_tags(tags);

        assert!(result.latest().is_some());
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v3.0.0"
        );
        Ok(())
    }

    #[test]
    fn test_process_tags_latest_per_major() -> TestResult {
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
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v1.2.0"
        );
        assert_eq!(
            result
                .latest_per_major()
                .get(&2)
                .ok_or("`latest_per_major().get(..)` is None")?
                .version()
                .to_string(),
            "v2.1.0"
        );
        Ok(())
    }

    #[test]
    fn test_process_tags_includes_prereleases_in_tags() -> TestResult {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0-beta.1", "bbb"),
            make_tag("v2.0.0-rc.1", "ccc"),
        ];
        let result = process_tags(tags);

        // Prereleases are included in semver_tags (filtered at query time)
        assert_eq!(result.semver_tags().len(), 3);
        // But "latest" only tracks stable releases
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        Ok(())
    }

    #[test]
    fn test_process_tags_non_semver_ignored() -> TestResult {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("latest", "bbb"),
            make_tag("main", "ccc"),
        ];
        let result = process_tags(tags);

        // Non-semver should be ignored
        assert_eq!(result.semver_tags().len(), 1);
        assert_eq!(
            result
                .latest()
                .ok_or("`result.latest()` is None")?
                .version()
                .to_string(),
            "v1.0.0"
        );
        Ok(())
    }

    #[test]
    fn git_base_follows_the_api_base() -> TestResult {
        let base = |api_base: &str, host: &str| {
            GitHubProvider::with_host(api_base, host, None).map(|provider| provider.git_base())
        };
        assert_eq!(
            base("https://api.github.com", "github.com")?,
            "https://github.com"
        );
        assert_eq!(
            base("https://ghe.corp.com/api/v3", "ghe.corp.com")?,
            "https://ghe.corp.com"
        );
        assert_eq!(
            base("http://127.0.0.1:8080/api/v3", "ghe.test")?,
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            base("https://custom.corp.com/github", "custom.corp.com")?,
            "https://custom.corp.com"
        );
        let provider =
            GitHubProvider::with_host("https://ghe.corp.com/api/v3", "ghe.corp.com", None)?;
        assert_eq!(
            provider.clone_url(&ActionRepoId::simple("o", "r")).ok(),
            Some("https://ghe.corp.com/o/r.git".to_string())
        );
        assert_eq!(
            provider.clone_url(&ActionRepoId::simple("o", "r+x")).ok(),
            Some("https://ghe.corp.com/o/r%2Bx.git".to_string())
        );
        Ok(())
    }

    #[test]
    fn test_graphql_endpoint_public_github() -> TestResult {
        let provider = GitHubProvider::new(None)?;
        assert_eq!(provider.graphql_endpoint(), crate::GITHUB_GRAPHQL_URL);
        Ok(())
    }

    #[test]
    fn test_graphql_endpoint_ghe() -> TestResult {
        let provider =
            GitHubProvider::with_host("https://ghe.corp.com/api/v3", "ghe.corp.com", None)?;

        assert_eq!(
            provider.graphql_endpoint(),
            "https://ghe.corp.com/api/graphql"
        );
        Ok(())
    }

    #[test]
    fn test_graphql_endpoint_ghe_nonstandard() -> TestResult {
        let provider =
            GitHubProvider::with_host("https://custom.corp.com/github", "custom.corp.com", None)?;

        assert_eq!(
            provider.graphql_endpoint(),
            "https://custom.corp.com/github/api/graphql"
        );
        Ok(())
    }
}
