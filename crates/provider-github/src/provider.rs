//! GitHub provider implementation

use crate::client::GitHubClient;
use actup_core::provider::{ActionYml, Provider, ProviderKind, Sha40};
use actup_core::version::{BranchInfo, Version, VersionInfo, VersionSet};
use actup_core::{ActionRepoId, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;
use tracing::debug;

/// GitHub provider for resolving action versions
pub struct GitHubProvider {
    client: GitHubClient,
    host: String,
}

impl GitHubProvider {
    /// Create a new GitHub provider for github.com
    pub fn new(token: Option<&str>) -> Result<Self> {
        let client = GitHubClient::new(token)?;
        Ok(Self {
            host: crate::GITHUB_HOST.to_string(),
            client,
        })
    }

    /// Create a GitHub provider for a custom host (e.g., GitHub Enterprise)
    pub fn with_host(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let client = GitHubClient::with_api_base(api_base, host, token)?;
        Ok(Self {
            host: host.to_string(),
            client,
        })
    }

    /// Parse tags into VersionInfo, filtering and sorting
    fn process_tags(&self, tags: Vec<crate::types::GitHubTag>) -> VersionSet {
        let mut semver_tags = Vec::new();
        let mut latest_per_major: HashMap<u32, VersionInfo> = HashMap::new();
        let mut latest: Option<VersionInfo> = None;

        for tag in tags {
            let version = Version::parse(&tag.name);
            let info = VersionInfo::new(version.clone(), &tag.commit.sha);

            if let Version::Semver {
                major, prerelease, ..
            } = &version
            {
                // Skip prereleases for "latest" calculations
                if prerelease.is_none() {
                    // Determine destinations first to minimize clones
                    let dominated_by_latest = latest.as_ref().is_some_and(|l| l.version >= version);
                    let dominated_by_major = latest_per_major
                        .get(major)
                        .is_some_and(|c| c.version >= version);

                    // Always goes in semver_tags - clone only if also needed elsewhere
                    if !dominated_by_latest && !dominated_by_major {
                        // New latest AND new latest for major
                        latest_per_major.insert(*major, info.clone());
                        latest = Some(info.clone());
                        semver_tags.push(info);
                    } else if !dominated_by_major {
                        // New latest for this major only
                        latest_per_major.insert(*major, info.clone());
                        semver_tags.push(info);
                    } else if !dominated_by_latest {
                        // New overall latest only
                        latest = Some(info.clone());
                        semver_tags.push(info);
                    } else {
                        // Just add to tags, no cloning needed
                        semver_tags.push(info);
                    }
                }
            }
        }

        // Sort semver tags descending
        semver_tags.sort_by(|a, b| b.version.cmp(&a.version));

        VersionSet {
            semver_tags,
            latest_per_major,
            latest,
            default_branch: None,
            fetched_at: Some(SystemTime::now()),
        }
    }
}

impl Provider for GitHubProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHub
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<VersionSet>> + Send + '_>> {
        let owner = repo.owner.clone();
        let repo_name = repo.repo.clone();

        Box::pin(async move {
            debug!(owner = %owner, repo = %repo_name, "fetching versions from GitHub");

            // Get tags
            let tags = self.client.list_tags(&owner, &repo_name, None).await?;

            // Process into VersionSet
            let mut version_set = self.process_tags(tags);

            // Get default branch info
            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            let branch = self
                .client
                .get_branch(&owner, &repo_name, &repo_info.default_branch)
                .await?;

            version_set.default_branch = Some(BranchInfo {
                name: repo_info.default_branch,
                sha: branch.commit.sha,
            });

            Ok(version_set)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
        let owner = repo.owner.clone();
        let repo_name = repo.repo.clone();
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

            let sha = match git_ref {
                Ok(r) => {
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
                }
                Err(_) => {
                    // Try as a branch
                    let branch = self
                        .client
                        .get_branch(&owner, &repo_name, &ref_name)
                        .await?;
                    branch.commit.sha
                }
            };

            Sha40::new(&sha).ok_or_else(|| actup_core::Error::Internal {
                context: "resolve_ref_to_sha".to_string(),
                message: format!("GitHub returned invalid SHA: {sha}"),
            })
        })
    }

    fn get_default_branch(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let owner = repo.owner.clone();
        let repo_name = repo.repo.clone();

        Box::pin(async move {
            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            Ok(repo_info.default_branch)
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
        let owner = repo.owner.clone();
        let repo_name = repo.repo.clone();
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
    ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
        let owner = repo.owner.clone();
        let repo_name = repo.repo.clone();
        let ref_name = ref_name.to_string();
        let subpath = repo.subpath.clone();

        Box::pin(async move {
            // Determine the path to action.yml
            let path = match &subpath {
                Some(p) if p.ends_with("action.yml") || p.ends_with("action.yaml") => p.clone(),
                Some(p) => format!("{p}/action.yml"),
                None => "action.yml".to_string(),
            };

            let content = self
                .client
                .get_content(&owner, &repo_name, &path, Some(&ref_name))
                .await?;

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
                String::new()
            };

            // Basic parsing (would need yaml parser for full implementation)
            Ok(ActionYml {
                raw,
                name: None,
                description: None,
                author: None,
            })
        })
    }
}
