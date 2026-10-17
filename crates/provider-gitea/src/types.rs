//! Gitea API response types
//!
//! Gitea's API is similar to GitHub's but with some differences.
//! These types represent the JSON responses from Gitea's REST API.

use actup_providers::TagLike;
use serde::{Deserialize, Serialize};

/// A tag from the Gitea API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaTag {
    /// Tag name (e.g., "v1.0.0")
    pub name: String,
    /// Commit details
    pub commit: GiteaTagCommit,
}

impl TagLike for GiteaTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.commit.sha
    }
}

/// Commit information in tag response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaTagCommit {
    /// Commit SHA
    pub sha: String,
}

/// Repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaRepository {
    /// Default branch name
    pub default_branch: String,
}

/// The archive flag of a repository
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GiteaArchive {
    /// Whether the repository is archived
    #[serde(default)]
    pub archived: bool,
}

/// Branch information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaBranch {
    /// Latest commit on this branch
    pub commit: GiteaBranchCommit,
}

/// Commit information in branch response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaBranchCommit {
    /// Commit SHA
    pub id: String,
}

/// File/directory content from repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaContent {
    /// Content type: "file", "dir", "symlink", "submodule"
    #[serde(rename = "type")]
    pub content_type: String,
    /// Encoding (usually "base64" for files)
    pub encoding: Option<String>,
    /// File content (base64 encoded for files)
    pub content: Option<String>,
}

/// Commit information (full)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaCommit {
    /// Commit SHA
    pub sha: String,
}

/// API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaError {
    /// Error message
    pub message: Option<String>,
}
