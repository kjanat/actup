//! Provider trait and registry for host-agnostic action resolution
//!
//! Providers implement the interface for querying action versions from
//! different git hosting platforms (GitHub, GitLab, Gitea, etc.).

use crate::{ActionRepoId, Result, VersionSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// Identifies the type of provider
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// GitHub.com or GitHub Enterprise
    GitHub,
    /// GitLab.com or self-hosted GitLab
    GitLab,
    /// Gitea instances
    Gitea,
    /// Bitbucket
    Bitbucket,
    /// Local filesystem (for testing/dev)
    Local,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHub => write!(f, "github"),
            Self::GitLab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
            Self::Bitbucket => write!(f, "bitbucket"),
            Self::Local => write!(f, "local"),
        }
    }
}

impl ProviderKind {
    /// Get the cache namespace for this provider kind
    #[must_use]
    pub fn cache_namespace(&self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Gitea => "gitea",
            Self::Bitbucket => "bitbucket",
            Self::Local => "local",
        }
    }
}

/// A 40-character commit SHA
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha40(pub String);

impl Sha40 {
    /// Create a new Sha40, validating the format and normalizing to lowercase
    pub fn new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(Self(s.to_ascii_lowercase()))
        } else {
            None
        }
    }

    /// Get the SHA string
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha40 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parsed action.yml content
#[derive(Debug, Clone, Default)]
pub struct ActionYml {
    /// Raw YAML content
    pub raw: String,
    /// Action name from the file
    pub name: Option<String>,
    /// Action description
    pub description: Option<String>,
    /// Author if specified
    pub author: Option<String>,
}

/// Trait for action providers (GitHub, GitLab, etc.)
///
/// This is the core abstraction that allows actup-core to remain
/// host-agnostic. Implementations live in separate crates.
pub trait Provider: Send + Sync {
    /// Get the provider kind
    fn kind(&self) -> ProviderKind;

    /// Get the host this provider serves (e.g., "github.com")
    fn host(&self) -> &str;

    /// Get the cache namespace (usually same as kind)
    fn cache_namespace(&self) -> &str {
        self.kind().cache_namespace()
    }

    /// Get available versions for an action repository
    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<VersionSet>> + Send + '_>>;

    /// Resolve a ref (tag/branch) to a full commit SHA
    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>>;

    /// Get the default branch name for a repository
    fn get_default_branch(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;

    /// Get the README content (if available)
    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>>;

    /// Get the action.yml content
    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>>;
}

/// Registry for managing multiple providers
///
/// Resolves providers by host, with support for default provider
/// when no explicit host is specified in an action reference.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_host: Option<String>,
}

impl ProviderRegistry {
    /// Create a new empty registry
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the default host (used when ActionRef.host is None)
    pub fn set_default_host(&mut self, host: impl Into<String>) {
        self.default_host = Some(host.into());
    }

    /// Get the default host
    #[must_use]
    pub fn default_host(&self) -> Option<&str> {
        self.default_host.as_deref()
    }

    /// Register a provider
    pub fn register(&mut self, provider: Box<dyn Provider>) {
        self.providers.push(provider);
    }

    /// Find a provider for the given host
    ///
    /// If host is None, uses the default host.
    #[must_use]
    pub fn find(&self, host: Option<&str>) -> Option<&dyn Provider> {
        let target_host = host.or(self.default_host.as_deref())?;
        self.providers
            .iter()
            .find(|p| p.host() == target_host)
            .map(std::convert::AsRef::as_ref)
    }

    /// Find a provider for an ActionRepoId
    #[must_use]
    pub fn find_for_repo(&self, repo: &ActionRepoId) -> Option<&dyn Provider> {
        self.find(repo.host.as_deref())
    }

    /// Find a provider by kind (for GHE hosts without exact match)
    ///
    /// Returns the first provider matching the given kind.
    /// Useful as fallback when `find()` returns None for custom hosts.
    #[must_use]
    pub fn find_by_kind(&self, kind: ProviderKind) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|p| p.kind() == kind)
            .map(std::convert::AsRef::as_ref)
    }

    /// Get all registered providers
    #[must_use]
    pub fn providers(&self) -> &[Box<dyn Provider>] {
        &self.providers
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("default_host", &self.default_host)
            .field("provider_count", &self.providers.len())
            .finish()
    }
}
