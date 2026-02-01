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
    pub const fn cache_namespace(&self) -> &'static str {
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
    /// Input parameters
    pub inputs: std::collections::HashMap<String, ActionInput>,
    /// Output values
    pub outputs: std::collections::HashMap<String, ActionOutput>,
    /// How the action runs
    pub runs: Option<ActionRuns>,
}

/// An input parameter for an action
#[derive(Debug, Clone, Default)]
pub struct ActionInput {
    /// Description of the input
    pub description: Option<String>,
    /// Whether this input is required
    pub required: bool,
    /// Default value if not provided
    pub default: Option<String>,
    /// Deprecation message if deprecated
    pub deprecation_message: Option<String>,
}

/// An output value from an action
#[derive(Debug, Clone, Default)]
pub struct ActionOutput {
    /// Description of the output
    pub description: Option<String>,
    /// Value expression (for composite actions)
    pub value: Option<String>,
}

/// How the action executes
#[derive(Debug, Clone)]
pub struct ActionRuns {
    /// Runtime: "composite", "node20", "node16", "docker"
    pub using: String,
    /// Main entry point (for node/docker)
    pub main: Option<String>,
    /// Pre-run script
    pub pre: Option<String>,
    /// Post-run script
    pub post: Option<String>,
    /// Docker image (for docker actions)
    pub image: Option<String>,
    /// Steps (for composite actions)
    pub steps: Vec<ActionStep>,
}

/// A step in a composite action
#[derive(Debug, Clone)]
pub struct ActionStep {
    /// Step name
    pub name: Option<String>,
    /// Step ID for outputs
    pub id: Option<String>,
    /// Action to run (uses:)
    pub uses: Option<String>,
    /// Shell command (run:)
    pub run: Option<String>,
    /// Shell to use
    pub shell: Option<String>,
    /// Working directory
    pub working_directory: Option<String>,
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

    /// Find a provider for an `ActionRepoId`
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::pin::Pin;

    /// Mock provider for testing
    struct MockProvider {
        kind: ProviderKind,
        host: String,
    }

    impl MockProvider {
        fn new(kind: ProviderKind, host: &str) -> Self {
            Self {
                kind,
                host: host.to_string(),
            }
        }
    }

    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }

        fn host(&self) -> &str {
            &self.host
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> Pin<Box<dyn Future<Output = Result<VersionSet>> + Send + '_>> {
            Box::pin(async { Ok(VersionSet::default()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
            Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    // ProviderKind tests

    #[test]
    fn test_provider_kind_display() {
        assert_eq!(ProviderKind::GitHub.to_string(), "github");
        assert_eq!(ProviderKind::GitLab.to_string(), "gitlab");
        assert_eq!(ProviderKind::Gitea.to_string(), "gitea");
        assert_eq!(ProviderKind::Bitbucket.to_string(), "bitbucket");
        assert_eq!(ProviderKind::Local.to_string(), "local");
    }

    #[test]
    fn test_provider_kind_cache_namespace() {
        assert_eq!(ProviderKind::GitHub.cache_namespace(), "github");
        assert_eq!(ProviderKind::GitLab.cache_namespace(), "gitlab");
        assert_eq!(ProviderKind::Gitea.cache_namespace(), "gitea");
        assert_eq!(ProviderKind::Bitbucket.cache_namespace(), "bitbucket");
        assert_eq!(ProviderKind::Local.cache_namespace(), "local");
    }

    // Sha40 tests

    #[test]
    fn test_sha40_valid() {
        let sha = Sha40::new("abc123def456abc123def456abc123def456abc1");
        assert!(sha.is_some());
        assert_eq!(
            sha.unwrap().as_str(),
            "abc123def456abc123def456abc123def456abc1"
        );
    }

    #[test]
    fn test_sha40_normalizes_to_lowercase() {
        let sha = Sha40::new("ABC123DEF456ABC123DEF456ABC123DEF456ABC1");
        assert!(sha.is_some());
        assert_eq!(
            sha.unwrap().as_str(),
            "abc123def456abc123def456abc123def456abc1"
        );
    }

    #[test]
    fn test_sha40_invalid_length() {
        assert!(Sha40::new("abc123").is_none());
        assert!(Sha40::new("abc123def456abc123def456abc123def456abc1extra").is_none());
    }

    #[test]
    fn test_sha40_invalid_chars() {
        assert!(Sha40::new("xyz123def456abc123def456abc123def456abc1").is_none()); // 'x', 'y', 'z'
        assert!(Sha40::new("!@#$%^def456abc123def456abc123def456abc1").is_none());
    }

    #[test]
    fn test_sha40_display() {
        let sha = Sha40::new("abc123def456abc123def456abc123def456abc1").unwrap();
        assert_eq!(format!("{sha}"), "abc123def456abc123def456abc123def456abc1");
    }

    // ProviderRegistry tests

    #[test]
    fn test_registry_new_empty() {
        let registry = ProviderRegistry::new();
        assert!(registry.providers().is_empty());
        assert!(registry.default_host().is_none());
    }

    #[test]
    fn test_registry_set_default_host() {
        let mut registry = ProviderRegistry::new();
        registry.set_default_host("github.com");
        assert_eq!(registry.default_host(), Some("github.com"));
    }

    #[test]
    fn test_registry_register_and_find() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitLab,
            "gitlab.com",
        )));

        assert_eq!(registry.providers().len(), 2);

        let github = registry.find(Some("github.com"));
        assert!(github.is_some());
        assert_eq!(github.unwrap().host(), "github.com");

        let gitlab = registry.find(Some("gitlab.com"));
        assert!(gitlab.is_some());
        assert_eq!(gitlab.unwrap().host(), "gitlab.com");
    }

    #[test]
    fn test_registry_find_uses_default() {
        let mut registry = ProviderRegistry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));

        // host=None should use default
        let provider = registry.find(None);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().host(), "github.com");
    }

    #[test]
    fn test_registry_find_returns_none_for_unknown_host() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));

        assert!(registry.find(Some("unknown.com")).is_none());
    }

    #[test]
    fn test_registry_find_for_repo() {
        let mut registry = ProviderRegistry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));

        let repo = ActionRepoId {
            host: None,
            owner: "actions".to_string(),
            repo: "checkout".to_string(),
            subpath: None,
        };

        let provider = registry.find_for_repo(&repo);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().host(), "github.com");
    }

    #[test]
    fn test_registry_find_by_kind() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "ghe.corp.com",
        )));
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitLab,
            "gitlab.com",
        )));

        // find_by_kind returns first match
        let github = registry.find_by_kind(ProviderKind::GitHub);
        assert!(github.is_some());
        assert_eq!(github.unwrap().host(), "github.com");

        let gitlab = registry.find_by_kind(ProviderKind::GitLab);
        assert!(gitlab.is_some());
        assert_eq!(gitlab.unwrap().host(), "gitlab.com");

        let gitea = registry.find_by_kind(ProviderKind::Gitea);
        assert!(gitea.is_none());
    }

    #[test]
    fn test_registry_debug_format() {
        let mut registry = ProviderRegistry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(
            ProviderKind::GitHub,
            "github.com",
        )));

        let debug = format!("{registry:?}");
        assert!(debug.contains("ProviderRegistry"));
        assert!(debug.contains("github.com"));
        assert!(debug.contains("provider_count: 1"));
    }
}
