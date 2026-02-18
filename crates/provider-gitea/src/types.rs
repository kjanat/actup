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
    /// Commit SHA this tag points to
    pub id: String,
    /// Commit details
    pub commit: GiteaTagCommit,
    /// Message associated with the tag (None for lightweight tags)
    pub message: Option<String>,
    /// Tarball download URL
    pub tarball_url: String,
    /// Zipball download URL
    pub zipball_url: String,
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
    /// Commit URL
    pub url: String,
}

/// Repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaRepository {
    /// Repository ID
    pub id: u64,
    /// Repository name
    pub name: String,
    /// Full name (owner/repo)
    pub full_name: String,
    /// Repository description
    pub description: Option<String>,
    /// Is this repo empty?
    pub empty: bool,
    /// Is this repo private?
    pub private: bool,
    /// Is this repo a fork?
    pub fork: bool,
    /// Default branch name
    pub default_branch: String,
    /// Clone URL (HTTPS)
    pub clone_url: String,
    /// SSH clone URL
    pub ssh_url: String,
    /// HTML URL
    pub html_url: String,
    /// Owner information
    pub owner: GiteaUser,
}

/// User/organization information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaUser {
    /// User ID
    pub id: u64,
    /// Username/login
    pub login: String,
    /// Full name
    pub full_name: Option<String>,
    /// Email address
    pub email: Option<String>,
    /// Avatar URL
    pub avatar_url: String,
}

/// Branch information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaBranch {
    /// Branch name
    pub name: String,
    /// Latest commit on this branch
    pub commit: GiteaBranchCommit,
    /// Is this branch protected?
    #[serde(default)]
    pub protected: bool,
}

/// Commit information in branch response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaBranchCommit {
    /// Commit SHA
    pub id: String,
    /// Commit URL
    pub url: String,
}

/// File/directory content from repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaContent {
    /// Content type: "file", "dir", "symlink", "submodule"
    #[serde(rename = "type")]
    pub content_type: String,
    /// Encoding (usually "base64" for files)
    pub encoding: Option<String>,
    /// File size in bytes
    pub size: u64,
    /// File name
    pub name: String,
    /// Full path from repository root
    pub path: String,
    /// File content (base64 encoded for files)
    pub content: Option<String>,
    /// SHA of the content
    pub sha: String,
    /// API URL for this content
    pub url: String,
    /// HTML URL for this content
    pub html_url: String,
    /// Git URL
    pub git_url: String,
    /// Download URL
    pub download_url: Option<String>,
}

/// Git ref information (for resolving refs to SHAs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaRef {
    /// Ref name (e.g., "refs/heads/main", "refs/tags/v1.0.0")
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// API URL
    pub url: String,
    /// Object this ref points to
    pub object: GiteaRefObject,
}

/// Object referenced by a git ref
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaRefObject {
    /// Object type: "commit", "tag", "tree", "blob"
    #[serde(rename = "type")]
    pub object_type: String,
    /// SHA of the object
    pub sha: String,
    /// API URL for the object
    pub url: String,
}

/// Commit information (full)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaCommit {
    /// Commit SHA
    pub sha: String,
    /// Commit URL
    pub url: String,
    /// HTML URL
    pub html_url: String,
    /// Commit details
    pub commit: GiteaCommitDetails,
}

/// Detailed commit information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaCommitDetails {
    /// Commit message
    pub message: String,
    /// Author information
    pub author: GiteaCommitAuthor,
    /// Committer information
    pub committer: GiteaCommitAuthor,
}

/// Author/committer information in commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaCommitAuthor {
    /// Name
    pub name: String,
    /// Email
    pub email: String,
    /// Timestamp
    pub date: String,
}

/// API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GiteaError {
    /// Error message
    pub message: Option<String>,
    /// HTTP URL for more information
    pub url: Option<String>,
}
