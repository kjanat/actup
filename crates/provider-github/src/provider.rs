//! GitHub provider implementation

use crate::client::GitHubClient;
use actup_core::provider::{
    ActionInput, ActionOutput, ActionRuns, ActionStep, ActionYml, Provider, ProviderKind, Sha40,
};
use actup_core::version::{BranchInfo, Version, VersionInfo, VersionSet};
use actup_core::{ActionRepoId, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;
use tracing::{debug, warn};

/// GitHub provider for resolving action versions
pub struct GitHubProvider {
    client: GitHubClient,
    host: String,
}

impl GitHubProvider {
    /// Create a new GitHub provider for github.com
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        let client = GitHubClient::new(token)?;
        Ok(Self {
            host: crate::GITHUB_HOST.to_string(),
            client,
        })
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
        })
    }

    /// Parse tags into `VersionInfo`, filtering and sorting
    fn process_tags(tags: Vec<crate::types::GitHubTag>) -> VersionSet {
        let mut semver_tags = Vec::new();
        let mut latest_per_major: HashMap<u32, VersionInfo> = HashMap::new();
        let mut major_tags: HashMap<u32, VersionInfo> = HashMap::new();
        let mut latest: Option<VersionInfo> = None;

        for tag in tags {
            let version = Version::parse(&tag.name);
            let info = VersionInfo::new(version.clone(), &tag.commit.sha);

            if let Version::Semver {
                major,
                minor,
                patch,
                prerelease,
                ..
            } = &version
            {
                // Always add to semver_tags (prereleases are filtered at query time)
                semver_tags.push(info.clone());

                // Track major-only tags (v1, v2, v6 - no minor/patch)
                if minor.is_none() && patch.is_none() && prerelease.is_none() {
                    major_tags.insert(*major, info.clone());
                }

                // Only update "latest" trackers for non-prerelease versions
                // (prereleases are filtered at query time via allow_prerelease flag)
                if prerelease.is_none() {
                    // Determine destinations first to minimize clones
                    let dominated_by_latest = latest.as_ref().is_some_and(|l| l.version >= version);
                    let dominated_by_major = latest_per_major
                        .get(major)
                        .is_some_and(|c| c.version >= version);

                    // Update latest trackers, clone only when needed
                    if !dominated_by_latest && !dominated_by_major {
                        // New latest AND new latest for major
                        latest_per_major.insert(*major, info.clone());
                        latest = Some(info);
                    } else if !dominated_by_major {
                        // New latest for this major only
                        latest_per_major.insert(*major, info.clone());
                    } else if !dominated_by_latest {
                        // New overall latest only
                        latest = Some(info);
                    }
                }
            }
        }

        // Sort semver tags descending
        semver_tags.sort_by(|a, b| b.version.cmp(&a.version));

        VersionSet {
            semver_tags,
            latest_per_major,
            major_tags,
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
            let mut version_set = Self::process_tags(tags);

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
                warn!(
                    owner = %owner,
                    repo = %repo_name,
                    "GitHub returned content without base64 encoding"
                );
                String::new()
            };

            // Parse the YAML content
            parse_action_yml(&raw)
        })
    }
}

/// Raw deserialization types for action.yml
#[derive(Debug, Deserialize, Default)]
struct RawActionYml {
    name: Option<String>,
    description: Option<String>,
    author: Option<String>,
    #[serde(default)]
    inputs: HashMap<String, RawInput>,
    #[serde(default)]
    outputs: HashMap<String, RawOutput>,
    runs: Option<RawRuns>,
}

#[derive(Debug, Deserialize, Default)]
struct RawInput {
    description: Option<String>,
    #[serde(default)]
    required: bool,
    default: Option<String>,
    #[serde(rename = "deprecationMessage")]
    deprecation_message: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawOutput {
    description: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRuns {
    using: String,
    main: Option<String>,
    pre: Option<String>,
    post: Option<String>,
    image: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Debug, Deserialize, Default)]
struct RawStep {
    name: Option<String>,
    id: Option<String>,
    uses: Option<String>,
    run: Option<String>,
    shell: Option<String>,
    #[serde(rename = "working-directory")]
    working_directory: Option<String>,
}

/// Parse action.yml content into `ActionYml` struct
#[allow(clippy::cognitive_complexity)]
fn parse_action_yml(raw: &str) -> Result<ActionYml> {
    let parsed: RawActionYml =
        serde_yaml_ng::from_str(raw).map_err(|e| actup_core::Error::YamlParse {
            path: "action.yml".into(),
            reason: e.to_string(),
        })?;

    let inputs = parsed
        .inputs
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                ActionInput {
                    description: v.description,
                    required: v.required,
                    default: v.default,
                    deprecation_message: v.deprecation_message,
                },
            )
        })
        .collect();

    let outputs = parsed
        .outputs
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                ActionOutput {
                    description: v.description,
                    value: v.value,
                },
            )
        })
        .collect();

    let runs = parsed.runs.map(|r| ActionRuns {
        using: r.using,
        main: r.main,
        pre: r.pre,
        post: r.post,
        image: r.image,
        steps: r
            .steps
            .into_iter()
            .map(|s| ActionStep {
                name: s.name,
                id: s.id,
                uses: s.uses,
                run: s.run,
                shell: s.shell,
                working_directory: s.working_directory,
            })
            .collect(),
    });

    Ok(ActionYml {
        raw: raw.to_string(),
        name: parsed.name,
        description: parsed.description,
        author: parsed.author,
        inputs,
        outputs,
        runs,
    })
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
        let result = GitHubProvider::process_tags(vec![]);
        assert!(result.semver_tags.is_empty());
        assert!(result.latest.is_none());
        assert!(result.latest_per_major.is_empty());
    }

    #[test]
    fn test_process_tags_single_version() {
        let tags = vec![make_tag("v1.0.0", "aaa111")];
        let result = GitHubProvider::process_tags(tags);

        assert_eq!(result.semver_tags.len(), 1);
        assert!(result.latest.is_some());
        assert_eq!(
            result.latest.as_ref().unwrap().version.to_string(),
            "v1.0.0"
        );
        assert_eq!(result.latest_per_major.len(), 1);
        assert!(result.latest_per_major.contains_key(&1));
    }

    #[test]
    fn test_process_tags_multiple_versions_sorted() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0", "bbb"),
            make_tag("v1.5.0", "ccc"),
        ];
        let result = GitHubProvider::process_tags(tags);

        // Should be sorted descending
        assert_eq!(result.semver_tags.len(), 3);
        assert_eq!(result.semver_tags[0].version.to_string(), "v2.0.0");
        assert_eq!(result.semver_tags[1].version.to_string(), "v1.5.0");
        assert_eq!(result.semver_tags[2].version.to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_latest_tracking() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("v2.0.0", "bbb"),
            make_tag("v3.0.0", "ccc"),
        ];
        let result = GitHubProvider::process_tags(tags);

        assert!(result.latest.is_some());
        assert_eq!(result.latest.unwrap().version.to_string(), "v3.0.0");
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
        let result = GitHubProvider::process_tags(tags);

        assert_eq!(result.latest_per_major.len(), 2);
        assert_eq!(
            result.latest_per_major.get(&1).unwrap().version.to_string(),
            "v1.2.0"
        );
        assert_eq!(
            result.latest_per_major.get(&2).unwrap().version.to_string(),
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
        let result = GitHubProvider::process_tags(tags);

        // Prereleases are included in semver_tags (filtered at query time)
        assert_eq!(result.semver_tags.len(), 3);
        // But "latest" only tracks stable releases
        assert_eq!(result.latest.unwrap().version.to_string(), "v1.0.0");
    }

    #[test]
    fn test_process_tags_non_semver_ignored() {
        let tags = vec![
            make_tag("v1.0.0", "aaa"),
            make_tag("latest", "bbb"),
            make_tag("main", "ccc"),
        ];
        let result = GitHubProvider::process_tags(tags);

        // Non-semver should be ignored
        assert_eq!(result.semver_tags.len(), 1);
        assert_eq!(result.latest.unwrap().version.to_string(), "v1.0.0");
    }

    // parse_action_yml tests

    #[test]
    fn test_parse_action_yml_minimal() {
        let yaml = r"
name: Test Action
description: A test action
";
        let result = parse_action_yml(yaml).unwrap();
        assert_eq!(result.name, Some("Test Action".to_string()));
        assert_eq!(result.description, Some("A test action".to_string()));
        assert!(result.author.is_none());
    }

    #[test]
    fn test_parse_action_yml_with_inputs() {
        let yaml = r#"
name: Test
description: Test
inputs:
  token:
    description: The GitHub token
    required: true
  timeout:
    description: Timeout in seconds
    required: false
    default: "30"
"#;
        let result = parse_action_yml(yaml).unwrap();
        assert_eq!(result.inputs.len(), 2);

        let token = result.inputs.get("token").unwrap();
        assert_eq!(token.description, Some("The GitHub token".to_string()));
        assert!(token.required);
        assert!(token.default.is_none());

        let timeout = result.inputs.get("timeout").unwrap();
        assert!(!timeout.required);
        assert_eq!(timeout.default, Some("30".to_string()));
    }

    #[test]
    fn test_parse_action_yml_with_outputs() {
        let yaml = r"
name: Test
description: Test
outputs:
  result:
    description: The result value
    value: ${{ steps.run.outputs.result }}
";
        let result = parse_action_yml(yaml).unwrap();
        assert_eq!(result.outputs.len(), 1);

        let output = result.outputs.get("result").unwrap();
        assert_eq!(output.description, Some("The result value".to_string()));
        assert!(output.value.is_some());
    }

    #[test]
    fn test_parse_action_yml_node_action() {
        let yaml = r"
name: Node Action
description: A Node.js action
runs:
  using: node20
  main: dist/index.js
";
        let result = parse_action_yml(yaml).unwrap();
        assert!(result.runs.is_some());

        let runs = result.runs.unwrap();
        assert_eq!(runs.using, "node20");
        assert_eq!(runs.main, Some("dist/index.js".to_string()));
    }

    #[test]
    fn test_parse_action_yml_composite_action() {
        let yaml = r#"
name: Composite Action
description: A composite action
runs:
  using: composite
  steps:
    - name: Checkout
      uses: actions/checkout@v4
    - name: Run script
      run: echo "Hello"
      shell: bash
"#;
        let result = parse_action_yml(yaml).unwrap();
        assert!(result.runs.is_some());

        let runs = result.runs.unwrap();
        assert_eq!(runs.using, "composite");
        assert_eq!(runs.steps.len(), 2);

        assert_eq!(runs.steps[0].name, Some("Checkout".to_string()));
        assert_eq!(runs.steps[0].uses, Some("actions/checkout@v4".to_string()));

        assert_eq!(runs.steps[1].name, Some("Run script".to_string()));
        assert_eq!(runs.steps[1].run, Some("echo \"Hello\"".to_string()));
        assert_eq!(runs.steps[1].shell, Some("bash".to_string()));
    }

    #[test]
    fn test_parse_action_yml_docker_action() {
        let yaml = r"
name: Docker Action
description: A Docker action
runs:
  using: docker
  image: Dockerfile
";
        let result = parse_action_yml(yaml).unwrap();
        assert!(result.runs.is_some());

        let runs = result.runs.unwrap();
        assert_eq!(runs.using, "docker");
        assert_eq!(runs.image, Some("Dockerfile".to_string()));
    }

    #[test]
    fn test_parse_action_yml_invalid_yaml() {
        let yaml = "name: [invalid";
        let result = parse_action_yml(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_preserves_raw() {
        let yaml = "name: Test\ndescription: Test";
        let result = parse_action_yml(yaml).unwrap();
        assert_eq!(result.raw, yaml);
    }
}
