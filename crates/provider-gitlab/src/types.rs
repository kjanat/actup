//! GitLab API response types

use actup_providers::TagLike;
use serde::{Deserialize, Serialize};

/// A tag from the GitLab API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabTag {
    /// Tag name (e.g., "v1.0.0")
    pub name: String,
    /// Target commit ID
    pub target: String,
    /// Commit details
    pub commit: Option<GitLabCommit>,
    /// Release information if this is a release tag
    pub release: Option<GitLabRelease>,
}

impl TagLike for GitLabTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        // Prefer commit.id if available, fall back to target
        self.commit.as_ref().map_or(&self.target, |c| &c.id)
    }
}

/// Commit information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabCommit {
    /// Full commit SHA
    pub id: String,
    /// Short commit SHA
    pub short_id: String,
    /// Commit title
    pub title: String,
    /// Commit message
    pub message: String,
    /// Author name
    pub author_name: String,
    /// Commit timestamp
    pub created_at: String,
}

/// Release information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabRelease {
    /// Release tag name
    pub tag_name: String,
    /// Release description
    pub description: Option<String>,
}

/// Project (repository) information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabProject {
    /// Project ID
    pub id: u64,
    /// Project name
    pub name: String,
    /// Project path (slug)
    pub path: String,
    /// Full path including namespace
    pub path_with_namespace: String,
    /// Default branch name
    pub default_branch: String,
    /// Project description
    pub description: Option<String>,
    /// Web URL
    pub web_url: String,
    /// Whether the project is archived
    #[serde(default)]
    pub archived: bool,
}

/// Branch information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabBranch {
    /// Branch name
    pub name: String,
    /// Latest commit on this branch
    pub commit: GitLabBranchCommit,
    /// Whether this is the default branch
    #[serde(default)]
    pub default: bool,
}

/// Simplified commit info in branch response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabBranchCommit {
    /// Commit SHA
    pub id: String,
    /// Short SHA
    pub short_id: String,
    /// Commit title
    pub title: String,
}

/// File content from repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabFile {
    /// File name
    pub file_name: String,
    /// File path
    pub file_path: String,
    /// File size in bytes
    pub size: u64,
    /// Encoding (usually "base64")
    pub encoding: String,
    /// File content (base64 encoded)
    pub content: String,
    /// Blob ID
    pub blob_id: String,
    /// Ref (branch/tag)
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Commit ID
    pub commit_id: String,
}

/// API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabError {
    /// Error message
    pub message: Option<String>,
    /// Detailed error description
    pub error: Option<String>,
    /// Error description (alternative field)
    pub error_description: Option<String>,
}
