//! GitHub-specific types for API responses

use actup_providers::TagLike;
use serde::Deserialize;

/// A git tag from the GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubTag {
    /// Tag name (e.g., "v1.2.3")
    pub name: String,

    /// Commit info
    pub commit: TagCommit,
}

/// Commit reference in a tag
#[derive(Debug, Clone, Deserialize)]
pub struct TagCommit {
    /// Commit SHA
    pub sha: String,
}

impl TagLike for GitHubTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.commit.sha
    }
}

/// Repository info from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRepo {
    /// Full name (owner/repo)
    pub full_name: String,

    /// Default branch name
    pub default_branch: String,

    /// Whether the repo is archived
    #[serde(default)]
    pub archived: bool,
}

/// A branch from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubBranch {
    /// Branch name
    pub name: String,

    /// Commit info
    pub commit: BranchCommit,
}

/// Commit reference in a branch
#[derive(Debug, Clone, Deserialize)]
pub struct BranchCommit {
    /// Commit SHA
    pub sha: String,
}

/// A commit from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubCommit {
    /// Full commit SHA
    pub sha: String,
}

/// A git reference from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRef {
    /// Ref name (e.g., "refs/tags/v1.0.0")
    #[serde(rename = "ref")]
    pub ref_name: String,

    /// Object info
    pub object: RefObject,
}

/// Object referenced by a git ref
#[derive(Debug, Clone, Deserialize)]
pub struct RefObject {
    /// Object type (commit, tag, etc.)
    #[serde(rename = "type")]
    pub object_type: String,

    /// Object SHA
    pub sha: String,
}

/// Rate limit info from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    /// Rate limit details
    pub rate: RateLimitInfo,
}

/// Rate limit details
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitInfo {
    /// Maximum requests per hour
    pub limit: u32,

    /// Remaining requests
    pub remaining: u32,

    /// Unix timestamp when limit resets
    pub reset: u64,
}

/// Content response from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubContent {
    /// Content type
    #[serde(rename = "type")]
    pub content_type: String,

    /// Base64 encoded content (for files)
    pub content: Option<String>,

    /// Encoding (usually "base64")
    pub encoding: Option<String>,

    /// File name
    pub name: String,

    /// File path
    pub path: String,

    /// SHA of the content
    pub sha: String,
}

/// An annotated tag object from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubTagObject {
    /// Tag name
    pub tag: String,
    /// Tag message
    pub message: Option<String>,
    /// Object this tag points to
    pub object: TagObjectTarget,
}

/// Target of an annotated tag
#[derive(Debug, Clone, Deserialize)]
pub struct TagObjectTarget {
    /// Object type (usually "commit")
    #[serde(rename = "type")]
    pub object_type: String,
    /// Object SHA
    pub sha: String,
}

/// Error response from GitHub API
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubError {
    /// Error message
    pub message: String,

    /// Documentation URL
    pub documentation_url: Option<String>,
}
