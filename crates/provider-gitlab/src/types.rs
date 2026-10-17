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
}

/// Project (repository) information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabProject {
    /// Default branch name
    pub default_branch: String,
    /// Whether the project is archived
    #[serde(default)]
    pub archived: bool,
}

/// Branch information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabBranch {
    /// Latest commit on this branch
    pub commit: GitLabBranchCommit,
}

/// Simplified commit info in branch response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabBranchCommit {
    /// Commit SHA
    pub id: String,
}

/// File content from repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabFile {
    /// Encoding (usually "base64")
    pub encoding: String,
    /// File content (base64 encoded)
    pub content: String,
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
