//! GraphQL client for batched GitHub API requests
//!
//! Fetches version information for multiple repositories in a single request.

use actup_core::version::{BranchInfo, Version, VersionInfo, VersionSet};
use actup_core::{ActionRepoId, Error, Result};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

/// GraphQL API endpoint
pub const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Default maximum tags to fetch per repository
pub const DEFAULT_MAX_TAGS_PER_REPO: u8 = 10;

/// GraphQL client for batched repository queries
#[derive(Debug, Clone)]
pub struct GraphQLClient {
    client: Client,
    endpoint: String,
}

impl GraphQLClient {
    /// Create a new GraphQL client (requires authentication)
    ///
    /// # Errors
    /// Returns error if HTTP client cannot be created or token is missing/invalid.
    pub fn new(token: &str) -> Result<Self> {
        Self::with_endpoint(GITHUB_GRAPHQL_URL, token)
    }

    /// User-Agent header value derived from crate version at compile time
    const USER_AGENT_VALUE: &'static str = concat!("actup/", env!("CARGO_PKG_VERSION"));

    /// Create a GraphQL client for a custom endpoint (GitHub Enterprise)
    ///
    /// # Errors
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_endpoint(endpoint: &str, token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(Self::USER_AGENT_VALUE));

        let auth_value = format!("Bearer {token}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value).map_err(|e| Error::Auth {
                provider: "github".to_string(),
                reason: format!("invalid token format: {e}"),
            })?,
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create GraphQL client".to_string(),
                message: e.to_string(),
            })?;

        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
        })
    }

    /// Fetch versions for multiple repositories in a single query
    ///
    /// # Arguments
    /// * `repos` - List of repositories to query
    /// * `max_tags` - Maximum tags per repo (stops early if stable release found)
    ///
    /// # Returns
    /// Map of repo -> version set. Missing repos have errors logged but don't fail the batch.
    ///
    /// # Errors
    /// Returns error if the GraphQL request fails (network, auth, or rate limit).
    pub async fn batch_get_versions(
        &self,
        repos: &[ActionRepoId],
        max_tags: u8,
    ) -> Result<HashMap<ActionRepoId, VersionSet>> {
        if repos.is_empty() {
            return Ok(HashMap::new());
        }

        // Clamp max_tags to API limit (GitHub GraphQL first: argument)
        let max_tags = max_tags.min(100);

        debug!(count = repos.len(), "fetching versions via GraphQL batch");

        let (query, valid_repos) = Self::build_query(repos, max_tags);

        // Early return if all repos were filtered out (invalid names)
        if valid_repos.is_empty() {
            debug!("no valid repos after filtering, skipping GraphQL request");
            return Ok(HashMap::new());
        }

        let response = self.execute_query(&query).await?;

        Ok(Self::parse_response(&valid_repos, &response))
    }

    /// Build GraphQL query for multiple repositories
    ///
    /// Returns the query string and the list of valid repos (filtered for injection safety).
    /// The `valid_repos` list has contiguous indices matching the alias numbering (repo0, repo1, ...).
    fn build_query(repos: &[ActionRepoId], max_tags: u8) -> (String, Vec<ActionRepoId>) {
        use std::fmt::Write;
        let mut query = String::from("query {\n");
        let mut valid_repos = Vec::new();

        for repo in repos {
            // Validate owner/repo to prevent GraphQL injection
            // GitHub names: alphanumeric, hyphen, underscore, dot (no quotes/backslashes)
            if !Self::is_valid_github_name(&repo.owner) || !Self::is_valid_github_name(&repo.repo) {
                warn!(
                    owner = %repo.owner,
                    repo = %repo.repo,
                    "skipping repo with invalid characters in owner/repo name"
                );
                continue;
            }

            // Use contiguous alias index based on valid repos count
            let i = valid_repos.len();
            valid_repos.push(repo.clone());

            // Use alias like repo0, repo1, etc.
            let _ = write!(
                query,
                r#"  repo{i}: repository(owner: "{owner}", name: "{repo}") {{
    defaultBranchRef {{
      name
      target {{
        ... on Commit {{ oid }}
      }}
    }}
    refs(refPrefix: "refs/tags/", first: {max_tags}, orderBy: {{field: TAG_COMMIT_DATE, direction: DESC}}) {{
      nodes {{
        name
        target {{
          ... on Commit {{ oid }}
          ... on Tag {{
            target {{
              ... on Commit {{ oid }}
            }}
          }}
        }}
      }}
    }}
  }}
"#,
                i = i,
                owner = repo.owner,
                repo = repo.repo,
                max_tags = max_tags,
            );
        }

        query.push('}');
        (query, valid_repos)
    }

    /// Validate GitHub owner/repo name (alphanumeric, hyphen, underscore, dot)
    fn is_valid_github_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    }

    /// Parse rate limit retry duration from response headers
    ///
    /// Checks `Retry-After` (seconds) and `X-RateLimit-Reset` (Unix timestamp).
    fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
        // Try Retry-After header first (seconds until retry)
        if let Some(retry_after) = headers.get("retry-after")
            && let Ok(secs) = retry_after.to_str().ok()?.parse::<u64>()
        {
            return Some(Duration::from_secs(secs));
        }

        // Try X-RateLimit-Reset header (Unix timestamp)
        if let Some(reset) = headers.get("x-ratelimit-reset")
            && let Ok(timestamp) = reset.to_str().ok()?.parse::<u64>()
        {
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            if timestamp > now {
                return Some(Duration::from_secs(timestamp - now));
            }
        }

        None
    }

    /// Execute GraphQL query
    async fn execute_query(&self, query: &str) -> Result<GraphQLResponse> {
        let request_body = GraphQLRequest { query };

        let response = self
            .client
            .post(&self.endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::Network {
                operation: "GraphQL request".to_string(),
                message: e.to_string(),
            })?;

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Auth {
                provider: "github".to_string(),
                reason: "GraphQL requires authentication - check your GITHUB_TOKEN".to_string(),
            });
        }

        // Handle 429 Too Many Requests - always rate limited
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = Self::parse_retry_after(response.headers());
            return Err(Error::RateLimited {
                provider: "github".to_string(),
                retry_after,
            });
        }

        // Handle 403 Forbidden - check headers to distinguish rate limit from permission denied
        if status == reqwest::StatusCode::FORBIDDEN {
            let headers = response.headers();
            let has_rate_limit_headers = headers.contains_key("retry-after")
                || headers.contains_key("x-ratelimit-reset")
                || headers.contains_key("x-ratelimit-remaining");

            if has_rate_limit_headers {
                let retry_after = Self::parse_retry_after(headers);
                return Err(Error::RateLimited {
                    provider: "github".to_string(),
                    retry_after,
                });
            }

            // No rate limit headers - this is a permission/scope issue
            let body = response.text().await.unwrap_or_default();
            let reason = if body.is_empty() {
                "insufficient permissions or token scope".to_string()
            } else {
                format!("insufficient permissions: {body}")
            };
            return Err(Error::Forbidden {
                provider: "github".to_string(),
                reason,
            });
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Network {
                operation: "GraphQL request".to_string(),
                message: format!("status {status}: {body}"),
            });
        }

        response.json().await.map_err(|e| Error::Network {
            operation: "parse GraphQL response".to_string(),
            message: e.to_string(),
        })
    }

    /// Parse GraphQL response into version sets
    fn parse_response(
        repos: &[ActionRepoId],
        response: &GraphQLResponse,
    ) -> HashMap<ActionRepoId, VersionSet> {
        let mut results = HashMap::new();

        // Check for GraphQL-level errors
        if let Some(errors) = &response.errors {
            for err in errors {
                warn!(message = %err.message, "GraphQL error");
            }
        }

        let Some(data) = &response.data else {
            return results;
        };

        for (i, repo) in repos.iter().enumerate() {
            let alias = format!("repo{i}");

            let Some(repo_data) = data.get(&alias) else {
                debug!(owner = %repo.owner, repo = %repo.repo, "repo not found in GraphQL response");
                continue;
            };

            // Handle null response (repo not found)
            if repo_data.is_null() {
                debug!(owner = %repo.owner, repo = %repo.repo, "repo returned null (not found or no access)");
                continue;
            }

            let repo_info: RepositoryData = match serde_json::from_value(repo_data.clone()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(owner = %repo.owner, repo = %repo.repo, error = %e, "failed to parse repo data");
                    continue;
                }
            };

            let version_set = Self::build_version_set(&repo_info);
            results.insert(repo.clone(), version_set);
        }

        results
    }

    /// Convert repository data to [`VersionSet`]
    fn build_version_set(repo_data: &RepositoryData) -> VersionSet {
        let mut semver_tags = Vec::new();
        let mut latest_per_major: HashMap<u32, VersionInfo> = HashMap::new();
        let mut major_tags: HashMap<u32, VersionInfo> = HashMap::new();
        let mut latest: Option<VersionInfo> = None;

        if let Some(refs) = &repo_data.refs {
            for node in &refs.nodes {
                let sha = node.target_sha().unwrap_or_default();
                if sha.is_empty() {
                    continue;
                }

                let version = Version::parse(&node.name);
                let info = VersionInfo::new(version.clone(), &sha);

                if let Version::Semver {
                    major,
                    minor,
                    patch,
                    prerelease,
                    ..
                } = &version
                {
                    semver_tags.push(info.clone());

                    // Track major-only tags (v1, v2, v6 - no minor/patch)
                    if minor.is_none() && patch.is_none() && prerelease.is_none() {
                        major_tags.insert(*major, info.clone());
                    }

                    // Only track stable releases for "latest"
                    if prerelease.is_none() {
                        let dominated_by_latest =
                            latest.as_ref().is_some_and(|l| l.version >= version);
                        let dominated_by_major = latest_per_major
                            .get(major)
                            .is_some_and(|c| c.version >= version);

                        if !dominated_by_latest && !dominated_by_major {
                            // Version is greater than both overall latest and major-specific latest
                            latest_per_major.insert(*major, info.clone());
                            latest = Some(info);
                        } else if !dominated_by_major {
                            // Version is greater than major-specific latest but not overall latest
                            // (invariant: latest >= max(latest_per_major) so this is the only other case)
                            latest_per_major.insert(*major, info.clone());
                        }
                        // Note: the case (!dominated_by_latest && dominated_by_major) is unreachable
                        // because we maintain latest >= max(latest_per_major.values())
                    }
                }
            }
        }

        // Sort descending by version
        semver_tags.sort_by(|a, b| b.version.cmp(&a.version));

        // Extract default branch info
        let default_branch = repo_data.default_branch_ref.as_ref().map(|br| BranchInfo {
            name: br.name.clone(),
            sha: br.target_sha().unwrap_or_default(),
        });

        VersionSet {
            semver_tags,
            latest_per_major,
            major_tags,
            latest,
            default_branch,
            fetched_at: Some(SystemTime::now()),
        }
    }
}

// GraphQL request/response types

#[derive(Serialize)]
struct GraphQLRequest<'a> {
    query: &'a str,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse {
    data: Option<HashMap<String, serde_json::Value>>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryData {
    default_branch_ref: Option<RefNode>,
    refs: Option<RefsConnection>,
}

#[derive(Debug, Deserialize)]
struct RefsConnection {
    nodes: Vec<TagNode>,
}

#[derive(Debug, Deserialize)]
struct TagNode {
    name: String,
    target: Option<GitTarget>,
}

impl TagNode {
    fn target_sha(&self) -> Option<String> {
        self.target.as_ref()?.sha()
    }
}

#[derive(Debug, Deserialize)]
struct RefNode {
    name: String,
    target: Option<GitTarget>,
}

impl RefNode {
    fn target_sha(&self) -> Option<String> {
        self.target.as_ref()?.sha()
    }
}

/// Git object target - can be Commit or Tag (which points to Commit)
#[derive(Debug, Deserialize)]
struct GitTarget {
    oid: Option<String>,
    target: Option<Box<Self>>, // For annotated tags
}

impl GitTarget {
    /// Get the commit SHA (handles both direct commits and annotated tags)
    fn sha(&self) -> Option<String> {
        // Direct commit
        if let Some(oid) = &self.oid {
            return Some(oid.clone());
        }
        // Annotated tag -> deref to commit
        self.target.as_ref()?.oid.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_query_single_repo() {
        let repos = vec![ActionRepoId {
            host: None,
            owner: "actions".to_string(),
            repo: "checkout".to_string(),
            subpath: None,
        }];

        let (query, valid_repos) = GraphQLClient::build_query(&repos, 10);

        assert!(query.contains("repo0: repository"));
        assert!(query.contains(r#"owner: "actions""#));
        assert!(query.contains(r#"name: "checkout""#));
        assert!(query.contains("first: 10"));
        assert_eq!(valid_repos.len(), 1);
        assert_eq!(valid_repos[0].owner, "actions");
    }

    #[test]
    fn test_build_query_multiple_repos() {
        let repos = vec![
            ActionRepoId {
                host: None,
                owner: "actions".to_string(),
                repo: "checkout".to_string(),
                subpath: None,
            },
            ActionRepoId {
                host: None,
                owner: "dtolnay".to_string(),
                repo: "rust-toolchain".to_string(),
                subpath: None,
            },
        ];

        let (query, valid_repos) = GraphQLClient::build_query(&repos, 5);

        assert!(query.contains("repo0:"));
        assert!(query.contains("repo1:"));
        assert!(query.contains(r#"owner: "actions""#));
        assert!(query.contains(r#"owner: "dtolnay""#));
        assert_eq!(valid_repos.len(), 2);
    }

    #[test]
    fn test_git_target_sha_direct_commit() {
        let target = GitTarget {
            oid: Some("abc123".to_string()),
            target: None,
        };
        assert_eq!(target.sha(), Some("abc123".to_string()));
    }

    #[test]
    fn test_git_target_sha_annotated_tag() {
        let target = GitTarget {
            oid: None,
            target: Some(Box::new(GitTarget {
                oid: Some("def456".to_string()),
                target: None,
            })),
        };
        assert_eq!(target.sha(), Some("def456".to_string()));
    }

    #[test]
    fn test_is_valid_github_name() {
        // Valid names
        assert!(GraphQLClient::is_valid_github_name("actions"));
        assert!(GraphQLClient::is_valid_github_name("rust-lang"));
        assert!(GraphQLClient::is_valid_github_name("my_repo"));
        assert!(GraphQLClient::is_valid_github_name("repo.js"));
        assert!(GraphQLClient::is_valid_github_name("123"));

        // Invalid names
        assert!(!GraphQLClient::is_valid_github_name("")); // empty
        assert!(!GraphQLClient::is_valid_github_name("test\"injection")); // quote
        assert!(!GraphQLClient::is_valid_github_name("test\\escape")); // backslash
        assert!(!GraphQLClient::is_valid_github_name("test/path")); // slash
        assert!(!GraphQLClient::is_valid_github_name("test name")); // space
    }

    #[test]
    fn test_build_query_skips_invalid_repos() {
        // Test with invalid repo in the middle to verify contiguous alias indexing
        let repos = vec![
            ActionRepoId {
                host: None,
                owner: "valid".to_string(),
                repo: "repo".to_string(),
                subpath: None,
            },
            ActionRepoId {
                host: None,
                owner: "invalid\"owner".to_string(),
                repo: "repo".to_string(),
                subpath: None,
            },
            ActionRepoId {
                host: None,
                owner: "another-valid".to_string(),
                repo: "repo2".to_string(),
                subpath: None,
            },
        ];

        let (query, valid_repos) = GraphQLClient::build_query(&repos, 10);

        // Valid repos included with contiguous alias numbering (0, 1 - not 0, 2)
        assert!(
            query.contains(r#"repo0: repository(owner: "valid", name: "repo")"#),
            "first valid repo should have alias repo0"
        );
        assert!(
            query.contains(r#"repo1: repository(owner: "another-valid", name: "repo2")"#),
            "second valid repo should have alias repo1 (contiguous)"
        );

        // Invalid repo excluded - no "repo2:" alias (would exist if using original indices)
        assert!(
            !query.contains("repo2:"),
            "alias numbering should be contiguous, no repo2:"
        );
        // No reference to the invalid owner
        assert!(
            !query.contains("invalid"),
            "invalid owner should not appear in query"
        );

        // valid_repos should only contain the two valid repos
        assert_eq!(valid_repos.len(), 2);
        assert_eq!(valid_repos[0].owner, "valid");
        assert_eq!(valid_repos[1].owner, "another-valid");
    }
}
