//! GraphQL client for batched GitHub API requests
//!
//! Fetches version information for multiple repositories in a single request.

use std::{collections::HashMap, num::NonZeroU8, sync::Arc, time::Duration};

use actup_core::{
    action_ref::ActionRepoId,
    clock::{self, Clock},
    error::{Error, Result},
    version,
};
use actup_providers::{TagLike, process_tags};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// GraphQL API endpoint
pub const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// GraphQL client for batched repository queries
#[derive(Debug, Clone)]
pub struct GraphQLClient {
    client: Client,
    endpoint: String,
    clock: Arc<dyn Clock>,
}

impl GraphQLClient {
    /// Create a new GraphQL client (requires authentication, uses [`clock::System`]).
    ///
    /// # Errors
    /// Returns error if HTTP client cannot be created or token is missing/invalid.
    pub fn new(token: &str) -> Result<Self> {
        Self::with_endpoint(GITHUB_GRAPHQL_URL, token)
    }

    /// User-Agent header value derived from crate version at compile time
    const USER_AGENT_VALUE: &'static str = concat!("actup/", env!("CARGO_PKG_VERSION"));

    /// Create a GraphQL client for a custom endpoint (GitHub Enterprise, uses [`clock::System`]).
    ///
    /// # Errors
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_endpoint(endpoint: &str, token: &str) -> Result<Self> {
        Self::with_endpoint_and_clock(endpoint, token, Arc::new(clock::System))
    }

    /// Create a GraphQL client with a custom endpoint and [`Clock`].
    ///
    /// # Errors
    /// Returns error if HTTP client cannot be created or token is invalid.
    pub fn with_endpoint_and_clock(
        endpoint: &str,
        token: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(Self::USER_AGENT_VALUE));

        let auth_value = format!("Bearer {token}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value).map_err(|e| Error::Auth {
                provider: "github".to_string(),
                reason: "invalid token format".into(),
                source: Some(Arc::new(e)),
            })?,
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create GraphQL client".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            })?;

        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
            clock,
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
        max_tags: NonZeroU8,
    ) -> Result<HashMap<ActionRepoId, version::Set>> {
        if repos.is_empty() {
            return Ok(HashMap::new());
        }

        // Clamp max_tags to API limit (GitHub GraphQL first: argument)
        let max_tags = max_tags.get().min(100);

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
            if !Self::is_valid_github_name(repo.owner()) || !Self::is_valid_github_name(repo.repo())
            {
                warn!(
                    owner = %repo.owner(),
                    repo = %repo.repo(),
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
    releases(first: {max_tags}, orderBy: {{field: CREATED_AT, direction: DESC}}) {{
      nodes {{
        tagName
        tagCommit {{ oid }}
      }}
    }}
  }}
"#,
                i = i,
                owner = repo.owner(),
                repo = repo.repo(),
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

    /// Parse rate limit retry duration from response headers.
    ///
    /// Checks `Retry-After` (seconds) and `X-RateLimit-Reset` (Unix timestamp).
    /// Falls through to the next header if parsing fails.
    fn parse_retry_after(
        headers: &reqwest::header::HeaderMap,
        clock: &dyn Clock,
    ) -> Option<Duration> {
        // Try Retry-After header first (seconds until retry)
        if let Some(secs) = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
        {
            return Some(Duration::from_secs(secs));
        }

        // Try X-RateLimit-Reset header (Unix timestamp)
        if let Some(timestamp) = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            && let Ok(now) = clock.now().duration_since(std::time::UNIX_EPOCH)
        {
            let now_secs = now.as_secs();
            if timestamp > now_secs {
                return Some(Duration::from_secs(timestamp - now_secs));
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
                message: String::new(),
                source: Some(Arc::new(e)),
            })?;

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Auth {
                provider: "github".to_string(),
                reason: "GraphQL requires authentication - check your GITHUB_TOKEN".to_string(),
                source: None,
            });
        }

        // Handle 429 Too Many Requests - always rate limited
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = Self::parse_retry_after(response.headers(), &*self.clock);
            return Err(Error::RateLimited {
                provider: "github".to_string(),
                retry_after,
            });
        }

        // Handle 403 Forbidden - check headers to distinguish rate limit from permission denied
        //
        // NOTE: GitHub includes x-ratelimit-* headers on most responses, not just rate-limited ones.
        // A normal 200 might have "x-ratelimit-remaining: 58". We must check the VALUE, not just
        // presence. Rate limiting is indicated by:
        //   - "retry-after" header exists (definitive), OR
        //   - "x-ratelimit-remaining" == "0"
        if status == reqwest::StatusCode::FORBIDDEN {
            let headers = response.headers();
            let is_rate_limited = headers.contains_key("retry-after")
                || headers
                    .get("x-ratelimit-remaining")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|s| s == "0");

            if is_rate_limited {
                let retry_after = Self::parse_retry_after(headers, &*self.clock);
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
                source: None,
            });
        }

        response.json().await.map_err(|e| Error::Network {
            operation: "parse GraphQL response".to_string(),
            message: String::new(),
            source: Some(Arc::new(e)),
        })
    }

    /// Parse GraphQL response into version sets
    fn parse_response(
        repos: &[ActionRepoId],
        response: &GraphQLResponse,
    ) -> HashMap<ActionRepoId, version::Set> {
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
                debug!(owner = %repo.owner(), repo = %repo.repo(), "repo not found in GraphQL response");
                continue;
            };

            // Handle null response (repo not found)
            if repo_data.is_null() {
                debug!(owner = %repo.owner(), repo = %repo.repo(), "repo returned null (not found or no access)");
                continue;
            }

            let repo_info: RepositoryData = match serde_json::from_value(repo_data.clone()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "failed to parse repo data");
                    continue;
                }
            };

            let version_set = Self::build_version_set(&repo_info);
            results.insert(repo.clone(), version_set);
        }

        results
    }

    /// Convert repository data to [`version::Set`]
    ///
    /// Uses the shared [`process_tags`] function from `actup_providers::tags`
    /// to ensure consistent tag processing across all providers.
    fn build_version_set(repo_data: &RepositoryData) -> version::Set {
        // Merge git tags and releases into one name-keyed set. Tags win on
        // collision (their SHA is the ref's own commit); releases add versions
        // that aren't in the recent-tags window (or aren't tagged at HEAD).
        let mut by_name: std::collections::HashMap<String, ProcessableTag> =
            std::collections::HashMap::new();

        if let Some(refs) = repo_data.refs.as_ref() {
            for node in &refs.nodes {
                let Some(sha) = node.target_sha().filter(|s| !s.is_empty()) else {
                    continue;
                };
                by_name.insert(
                    node.name.clone(),
                    ProcessableTag {
                        name: node.name.clone(),
                        sha,
                    },
                );
            }
        }

        if let Some(releases) = repo_data.releases.as_ref() {
            for rel in &releases.nodes {
                let Some(name) = rel.tag_name.clone() else {
                    continue;
                };
                let Some(sha) = rel
                    .tag_commit
                    .as_ref()
                    .and_then(GitTarget::sha)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                by_name
                    .entry(name.clone())
                    .or_insert(ProcessableTag { name, sha });
            }
        }

        // Use shared tag processing logic
        let mut set = process_tags(by_name.into_values());

        // Set default branch if present (GraphQL-specific: fetched in same query)
        if let Some(br) = &repo_data.default_branch_ref
            && let Some(sha) = br.target_sha()
        {
            let branch = version::BranchInfo::new(br.name.clone(), sha);
            set.set_default_branch(branch);
        }

        set
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
    releases: Option<ReleasesConnection>,
}

#[derive(Debug, Deserialize)]
struct RefsConnection {
    nodes: Vec<TagNode>,
}

#[derive(Debug, Deserialize)]
struct ReleasesConnection {
    nodes: Vec<ReleaseNode>,
}

/// A GitHub Release. Many actions (e.g. `taiki-e/install-action`) version their
/// real releases here while their git tags are dominated by non-semver tags, so
/// releases are a second version source merged with tags.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseNode {
    tag_name: Option<String>,
    tag_commit: Option<GitTarget>,
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

/// Wrapper for processing GraphQL tags via the shared `process_tags` function.
///
/// GraphQL `TagNode` returns `Option<String>` for SHA (computed from nested response),
/// but `TagLike` requires `&str`. This wrapper pre-computes and stores the SHA.
struct ProcessableTag {
    name: String,
    sha: String,
}

impl TagLike for ProcessableTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.sha
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
        let repos = vec![ActionRepoId::simple("actions", "checkout")];

        let (query, valid_repos) = GraphQLClient::build_query(&repos, 10);

        assert!(query.contains("repo0: repository"));
        assert!(query.contains(r#"owner: "actions""#));
        assert!(query.contains(r#"name: "checkout""#));
        assert!(query.contains("first: 10"));
        assert!(query.contains("releases(first: 10"));
        assert!(query.contains("CREATED_AT"));
        assert_eq!(valid_repos.len(), 1);
        assert_eq!(valid_repos[0].owner(), "actions");
    }

    #[test]
    fn test_build_version_set_merges_releases() {
        // Tags are all non-semver (tool names, à la taiki-e/install-action),
        // but releases carry the real semver — which must surface in the set.
        let repo = RepositoryData {
            default_branch_ref: None,
            refs: Some(RefsConnection {
                nodes: vec![TagNode {
                    name: "zola".to_string(),
                    target: Some(GitTarget {
                        oid: Some("a".repeat(40)),
                        target: None,
                    }),
                }],
            }),
            releases: Some(ReleasesConnection {
                nodes: vec![ReleaseNode {
                    tag_name: Some("v2.82.3".to_string()),
                    tag_commit: Some(GitTarget {
                        oid: Some("b".repeat(40)),
                        target: None,
                    }),
                }],
            }),
        };

        let set = GraphQLClient::build_version_set(&repo);
        let Some(latest) = set.latest() else {
            panic!("release version should surface");
        };
        assert_eq!(latest.version().to_string(), "v2.82.3");
        assert_eq!(latest.sha(), "b".repeat(40));
    }

    #[test]
    fn test_build_query_multiple_repos() {
        let repos = vec![
            ActionRepoId::simple("actions", "checkout"),
            ActionRepoId::simple("dtolnay", "rust-toolchain"),
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
            ActionRepoId::simple("valid", "repo"),
            ActionRepoId::simple("invalid\"owner", "repo"),
            ActionRepoId::simple("another-valid", "repo2"),
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
        assert_eq!(valid_repos[0].owner(), "valid");
        assert_eq!(valid_repos[1].owner(), "another-valid");
    }
}
