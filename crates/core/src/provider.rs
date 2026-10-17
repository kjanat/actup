//! Provider trait and registry for host-agnostic action resolution
//!
//! Providers implement the interface for querying action versions from
//! different git hosting platforms (GitHub, GitLab, Gitea, etc.).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    ActionRepoId, CachingProvider, Config, Error, Registration, Result, Retention,
    compat::{BoxFuture, MaybeSend, MaybeSync},
    effective_host, resolve_default_host, validate_no_duplicate_presets, version,
};

/// Identifies the type of provider
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
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

impl fmt::Display for Kind {
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

impl Kind {
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sha40(String);

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

/// A repository that an action search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSummary {
    /// Repository owner or group.
    pub owner: String,
    /// Repository or project name.
    pub repo: String,
    /// The repository description, when it has one.
    pub description: Option<String>,
}

/// Whether a repository still serves its actions as maintained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoState {
    /// The repository is neither archived nor disabled.
    Active,
    /// The owner archived the repository; it is read-only.
    Archived,
    /// The host disabled the repository.
    Disabled,
}

/// A provider's answer to an action search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Search {
    /// The provider offers no action search.
    Unsupported,
    /// The repositories that match, best match first.
    Found(Vec<RepoSummary>),
}

/// Parsed action.yml content
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct ActionYml {
    /// Raw YAML content
    raw: String,
    /// Action name from the file
    name: Option<String>,
    /// Action description
    description: Option<String>,
    /// Author if specified
    author: Option<String>,
    /// Input parameters
    inputs: std::collections::HashMap<String, ActionInput>,
    /// Output values
    outputs: std::collections::HashMap<String, ActionOutput>,
    /// How the action runs
    runs: Option<ActionRuns>,
}

impl ActionYml {
    /// Create a new `ActionYml` with raw YAML content
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            ..Default::default()
        }
    }

    /// Get the raw YAML content
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Get the action name
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get the action description
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Get the author
    #[must_use]
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    /// Get the input parameters
    #[must_use]
    pub const fn inputs(&self) -> &std::collections::HashMap<String, ActionInput> {
        &self.inputs
    }

    /// Get the output values
    #[must_use]
    pub const fn outputs(&self) -> &std::collections::HashMap<String, ActionOutput> {
        &self.outputs
    }

    /// Get the runs configuration
    #[must_use]
    pub const fn runs(&self) -> Option<&ActionRuns> {
        self.runs.as_ref()
    }

    /// Builder: Set the action name
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builder: Set the description
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: Set the author
    #[must_use]
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Builder: Set the inputs
    #[must_use]
    pub fn with_inputs(mut self, inputs: std::collections::HashMap<String, ActionInput>) -> Self {
        self.inputs = inputs;
        self
    }

    /// Builder: Set the outputs
    #[must_use]
    pub fn with_outputs(
        mut self,
        outputs: std::collections::HashMap<String, ActionOutput>,
    ) -> Self {
        self.outputs = outputs;
        self
    }

    /// Builder: Set the runs configuration
    #[must_use]
    pub fn with_runs(mut self, runs: ActionRuns) -> Self {
        self.runs = Some(runs);
        self
    }
}

/// An input parameter for an action
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct ActionInput {
    /// Description of the input
    description: Option<String>,
    /// Whether this input is required
    required: bool,
    /// Default value if not provided
    default: Option<String>,
    /// Deprecation message if deprecated
    deprecation_message: Option<String>,
}

impl ActionInput {
    /// Create a new `ActionInput`
    #[must_use]
    pub const fn new() -> Self {
        Self {
            description: None,
            required: false,
            default: None,
            deprecation_message: None,
        }
    }

    /// Get the input description
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Check if this input is required
    #[must_use]
    pub const fn required(&self) -> bool {
        self.required
    }

    /// Get the default value
    #[must_use]
    pub fn default(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// Get the deprecation message
    #[must_use]
    pub fn deprecation_message(&self) -> Option<&str> {
        self.deprecation_message.as_deref()
    }

    /// Builder: Set the description
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: Set whether this input is required
    #[must_use]
    pub const fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Builder: Set the default value
    #[must_use]
    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    /// Builder: Set the deprecation message
    #[must_use]
    pub fn with_deprecation_message(mut self, message: impl Into<String>) -> Self {
        self.deprecation_message = Some(message.into());
        self
    }
}

/// An output value from an action
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct ActionOutput {
    /// Description of the output
    description: Option<String>,
    /// Value expression (for composite actions)
    value: Option<String>,
}

impl ActionOutput {
    /// Create a new `ActionOutput`
    #[must_use]
    pub const fn new() -> Self {
        Self {
            description: None,
            value: None,
        }
    }

    /// Get the output description
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Get the value expression
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// Builder: Set the description
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: Set the value expression
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
}

/// How the action executes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct ActionRuns {
    /// Runtime: "composite", "node24", "node22", "node20", "node16", "docker"
    using: String,
    /// Main entry point (for node/docker)
    main: Option<String>,
    /// Pre-run script
    pre: Option<String>,
    /// Post-run script
    post: Option<String>,
    /// Docker image (for docker actions)
    image: Option<String>,
    /// Steps (for composite actions)
    steps: Vec<ActionStep>,
}

impl ActionRuns {
    /// Create a new `ActionRuns` with the runtime
    #[must_use]
    pub fn new(using: impl Into<String>) -> Self {
        Self {
            using: using.into(),
            main: None,
            pre: None,
            post: None,
            image: None,
            steps: Vec::new(),
        }
    }

    /// Get the runtime
    #[must_use]
    pub fn using(&self) -> &str {
        &self.using
    }

    /// Get the main entry point
    #[must_use]
    pub fn main(&self) -> Option<&str> {
        self.main.as_deref()
    }

    /// Get the pre-run script
    #[must_use]
    pub fn pre(&self) -> Option<&str> {
        self.pre.as_deref()
    }

    /// Get the post-run script
    #[must_use]
    pub fn post(&self) -> Option<&str> {
        self.post.as_deref()
    }

    /// Get the Docker image
    #[must_use]
    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// Get the steps
    #[must_use]
    pub fn steps(&self) -> &[ActionStep] {
        &self.steps
    }

    /// Builder: Set the main entry point
    #[must_use]
    pub fn with_main(mut self, main: impl Into<String>) -> Self {
        self.main = Some(main.into());
        self
    }

    /// Builder: Set the pre-run script
    #[must_use]
    pub fn with_pre(mut self, pre: impl Into<String>) -> Self {
        self.pre = Some(pre.into());
        self
    }

    /// Builder: Set the post-run script
    #[must_use]
    pub fn with_post(mut self, post: impl Into<String>) -> Self {
        self.post = Some(post.into());
        self
    }

    /// Builder: Set the Docker image
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Builder: Set the steps
    #[must_use]
    pub fn with_steps(mut self, steps: Vec<ActionStep>) -> Self {
        self.steps = steps;
        self
    }
}

/// A step in a composite action
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct ActionStep {
    /// Step name
    name: Option<String>,
    /// Step ID for outputs
    id: Option<String>,
    /// Action to run (uses:)
    uses: Option<String>,
    /// Shell command (run:)
    run: Option<String>,
    /// Shell to use
    shell: Option<String>,
    /// Working directory
    working_directory: Option<String>,
}

impl ActionStep {
    /// Create a new `ActionStep`
    #[must_use]
    pub const fn new() -> Self {
        Self {
            name: None,
            id: None,
            uses: None,
            run: None,
            shell: None,
            working_directory: None,
        }
    }

    /// Get the step name
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get the step ID
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Get the uses value
    #[must_use]
    pub fn uses(&self) -> Option<&str> {
        self.uses.as_deref()
    }

    /// Get the run command
    #[must_use]
    pub fn run(&self) -> Option<&str> {
        self.run.as_deref()
    }

    /// Get the shell
    #[must_use]
    pub fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// Get the working directory
    #[must_use]
    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    /// Builder: Set the step name
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builder: Set the step ID
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Builder: Set the uses value
    #[must_use]
    pub fn with_uses(mut self, uses: impl Into<String>) -> Self {
        self.uses = Some(uses.into());
        self
    }

    /// Builder: Set the run command
    #[must_use]
    pub fn with_run(mut self, run: impl Into<String>) -> Self {
        self.run = Some(run.into());
        self
    }

    /// Builder: Set the shell
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    /// Builder: Set the working directory
    #[must_use]
    pub fn with_working_directory(mut self, working_directory: impl Into<String>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }
}

/// Trait for action providers (GitHub, GitLab, etc.)
///
/// This is the core abstraction that allows actup-core to remain
/// host-agnostic. Implementations live in separate crates.
pub trait Provider: MaybeSend + MaybeSync {
    /// Get the provider kind
    fn kind(&self) -> Kind;

    /// Whether this concrete provider can serve batched multi-repo lookups.
    ///
    /// Defaults to `false` so test doubles and non-GitHub providers do not
    /// accidentally opt into GitHub-specific batching paths.
    fn supports_batch_fetch(&self) -> bool {
        false
    }

    /// Get the host this provider serves (e.g., "github.com")
    fn host(&self) -> &str;

    /// Get the cache namespace (usually same as kind)
    fn cache_namespace(&self) -> &str {
        self.kind().cache_namespace()
    }

    /// Get available versions for an action repository
    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>>;

    /// Resolve a ref (tag/branch) to a full commit SHA
    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>>;

    /// Resolve a ref (tag/branch) to a full commit SHA without reusing a cached answer
    fn refresh_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        self.resolve_ref_to_sha(repo, ref_name)
    }

    /// Get the default branch name for a repository
    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>>;

    /// Get the README content (if available)
    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>>;

    /// Get the action.yml content
    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>>;

    /// Search the host for action repositories that match `query`
    fn search_actions(&self, _query: &str) -> BoxFuture<'_, Result<Search>> {
        Box::pin(async { Ok(Search::Unsupported) })
    }

    /// Get whether a repository is active, archived or disabled, or `None` when the provider does not report it
    fn get_repo_state(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<Option<RepoState>>> {
        Box::pin(async { Ok(None) })
    }
}

impl<P> Provider for Box<P>
where
    P: Provider + ?Sized,
{
    fn kind(&self) -> Kind {
        (**self).kind()
    }

    fn supports_batch_fetch(&self) -> bool {
        (**self).supports_batch_fetch()
    }

    fn host(&self) -> &str {
        (**self).host()
    }

    fn cache_namespace(&self) -> &str {
        (**self).cache_namespace()
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
        (**self).get_versions(repo)
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        (**self).resolve_ref_to_sha(repo, ref_name)
    }

    fn refresh_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        (**self).refresh_ref_to_sha(repo, ref_name)
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        (**self).get_default_branch(repo)
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        (**self).get_readme(repo, ref_name)
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>> {
        (**self).get_action_yml(repo, ref_name)
    }

    fn search_actions(&self, query: &str) -> BoxFuture<'_, Result<Search>> {
        (**self).search_actions(query)
    }

    fn get_repo_state(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<Option<RepoState>>> {
        (**self).get_repo_state(repo)
    }
}

/// Registry for managing multiple providers
///
/// Resolves providers by host, with support for default provider
/// when no explicit host is specified in an action reference.
#[derive(Default)]
pub struct Registry {
    providers: Vec<Box<dyn Provider>>,
    default_host: Option<String>,
}

impl Registry {
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
        self.find(repo.host())
    }

    /// Find a provider by kind (for GHE hosts without exact match)
    ///
    /// Returns the first provider matching the given kind.
    /// Useful as fallback when `find()` returns None for custom hosts.
    #[must_use]
    pub fn find_by_kind(&self, kind: Kind) -> Option<&dyn Provider> {
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

    /// Replace every registered provider with the provider `wrap` returns for it.
    #[must_use]
    pub fn map_providers(
        mut self,
        wrap: impl FnMut(Box<dyn Provider>) -> Box<dyn Provider>,
    ) -> Self {
        self.providers = self.providers.into_iter().map(wrap).collect();
        self
    }

    /// Build a registry for `config` from the providers that `registrations` construct.
    ///
    /// Registers every `[hosts]` entry and the default host, each behind a
    /// [`CachingProvider`] that reuses answers as long as `retention` allows.
    ///
    /// # Errors
    ///
    /// Returns an error if a host does not resolve or its provider cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if two registrations share a preset hostname.
    pub fn from_config(
        config: &Config,
        registrations: &[&Registration],
        retention: &Retention,
    ) -> Result<Self> {
        validate_no_duplicate_presets(registrations);
        let mut registry = Self::new();
        let default_host = resolve_default_host(config, registrations)?;
        registry.set_default_host(&default_host);
        for hostname in config.hosts.keys() {
            registry.register_host(hostname, config, registrations, retention)?;
        }
        if !config.hosts.contains_key(&default_host) {
            registry.register_host(&default_host, config, registrations, retention)?;
        }
        Ok(registry)
    }

    fn register_host(
        &mut self,
        hostname: &str,
        config: &Config,
        registrations: &[&Registration],
        retention: &Retention,
    ) -> Result<()> {
        let provider_config = effective_host(hostname, config, registrations, &config.fetch)?;
        let registration = registrations
            .iter()
            .find(|r| r.kind == provider_config.kind)
            .ok_or_else(|| Error::InvalidConfig {
                path: format!("host \"{hostname}\""),
                reason: format!("no registration for kind {:?}", provider_config.kind),
                hint: None,
                source: None,
            })?;
        let provider = (registration.create)(provider_config)?;
        self.register(Box::new(CachingProvider::with_retention(
            provider,
            retention.clone(),
        )));
        Ok(())
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("default_host", &self.default_host)
            .field("provider_count", &self.providers.len())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Mock provider for testing
    struct MockProvider {
        kind: Kind,
        host: String,
    }

    impl MockProvider {
        fn new(kind: Kind, host: &str) -> Self {
            Self {
                kind,
                host: host.to_string(),
            }
        }
    }

    impl Provider for MockProvider {
        fn kind(&self) -> Kind {
            self.kind
        }

        fn host(&self) -> &str {
            &self.host
        }

        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            if self.host == COUNTED_HOST {
                COUNTED_FETCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Box::pin(async { Ok(version::Set::default()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, Result<Sha40>> {
            Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
        }

        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    // Kind tests

    #[test]
    fn test_kind_display() {
        assert_eq!(Kind::GitHub.to_string(), "github");
        assert_eq!(Kind::GitLab.to_string(), "gitlab");
        assert_eq!(Kind::Gitea.to_string(), "gitea");
        assert_eq!(Kind::Bitbucket.to_string(), "bitbucket");
        assert_eq!(Kind::Local.to_string(), "local");
    }

    #[test]
    fn test_kind_cache_namespace() {
        assert_eq!(Kind::GitHub.cache_namespace(), "github");
        assert_eq!(Kind::GitLab.cache_namespace(), "gitlab");
        assert_eq!(Kind::Gitea.cache_namespace(), "gitea");
        assert_eq!(Kind::Bitbucket.cache_namespace(), "bitbucket");
        assert_eq!(Kind::Local.cache_namespace(), "local");
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

    // Registry tests

    #[test]
    fn test_registry_new_empty() {
        let registry = Registry::new();
        assert!(registry.providers().is_empty());
        assert!(registry.default_host().is_none());
    }

    #[test]
    fn test_registry_set_default_host() {
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        assert_eq!(registry.default_host(), Some("github.com"));
    }

    #[test]
    fn test_registry_register_and_find() {
        let mut registry = Registry::new();
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));
        registry.register(Box::new(MockProvider::new(Kind::GitLab, "gitlab.com")));

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
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));

        // host=None should use default
        let provider = registry.find(None);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().host(), "github.com");
    }

    #[test]
    fn test_registry_find_returns_none_for_unknown_host() {
        let mut registry = Registry::new();
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));

        assert!(registry.find(Some("unknown.com")).is_none());
    }

    #[test]
    fn test_registry_find_for_repo() {
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));

        let repo = ActionRepoId::simple("actions", "checkout");

        let provider = registry.find_for_repo(&repo);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().host(), "github.com");
    }

    #[test]
    fn test_registry_find_by_kind() {
        let mut registry = Registry::new();
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "ghe.corp.com")));
        registry.register(Box::new(MockProvider::new(Kind::GitLab, "gitlab.com")));

        // find_by_kind returns first match
        let github = registry.find_by_kind(Kind::GitHub);
        assert!(github.is_some());
        assert_eq!(github.unwrap().host(), "github.com");

        let gitlab = registry.find_by_kind(Kind::GitLab);
        assert!(gitlab.is_some());
        assert_eq!(gitlab.unwrap().host(), "gitlab.com");

        let gitea = registry.find_by_kind(Kind::Gitea);
        assert!(gitea.is_none());
    }

    #[test]
    fn test_registry_map_providers_keeps_hosts_and_default() {
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));
        registry.register(Box::new(MockProvider::new(Kind::GitLab, "gitlab.com")));

        let mut seen = Vec::new();
        let registry = registry.map_providers(|provider| {
            seen.push(provider.host().to_string());
            Box::new(MockProvider::new(provider.kind(), "wrapped.example"))
        });

        assert_eq!(seen, ["github.com", "gitlab.com"]);
        assert_eq!(registry.default_host(), Some("github.com"));
        let hosts: Vec<&str> = registry.providers().iter().map(Provider::host).collect();
        assert_eq!(hosts, ["wrapped.example", "wrapped.example"]);
    }

    #[test]
    fn test_registry_debug_format() {
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(MockProvider::new(Kind::GitHub, "github.com")));

        let debug = format!("{registry:?}");
        assert!(debug.contains("Registry"));
        assert!(debug.contains("github.com"));
        assert!(debug.contains("provider_count: 1"));
    }

    #[test]
    fn test_kind_serde_round_trip() {
        for kind in [
            Kind::GitHub,
            Kind::GitLab,
            Kind::Gitea,
            Kind::Bitbucket,
            Kind::Local,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let deserialized: Kind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, deserialized);
        }
    }

    #[test]
    fn test_sha40_serde_round_trip() {
        let sha = Sha40::new("abc123def456abc123def456abc123def456abcd").unwrap();
        let json = serde_json::to_string(&sha).unwrap();
        let deserialized: Sha40 = serde_json::from_str(&json).unwrap();
        assert_eq!(sha, deserialized);
    }

    #[test]
    fn test_action_yml_serde_round_trip() {
        let yml = ActionYml::new("raw: content")
            .with_name("My Action")
            .with_description("Does things")
            .with_author("test")
            .with_runs(ActionRuns::new("node20").with_main("index.js"));
        let json = serde_json::to_string(&yml).unwrap();
        let deserialized: ActionYml = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name(), Some("My Action"));
        assert_eq!(deserialized.description(), Some("Does things"));
        assert_eq!(deserialized.author(), Some("test"));
        assert_eq!(deserialized.raw(), "raw: content");
        assert_eq!(deserialized.runs().unwrap().using(), "node20");
        assert_eq!(deserialized.runs().unwrap().main(), Some("index.js"));
    }

    #[test]
    fn test_action_yml_with_inputs_outputs_serde_round_trip() {
        let mut inputs = std::collections::HashMap::new();
        inputs.insert(
            "token".to_string(),
            ActionInput::new()
                .with_description("GitHub token")
                .with_required(true)
                .with_default("${{ github.token }}"),
        );
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "result".to_string(),
            ActionOutput::new()
                .with_description("The result")
                .with_value("steps.run.outputs.result"),
        );
        let yml = ActionYml::new("raw")
            .with_inputs(inputs)
            .with_outputs(outputs);
        let json = serde_json::to_string(&yml).unwrap();
        let deserialized: ActionYml = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.inputs().len(), 1);
        assert!(deserialized.inputs()["token"].required());
        assert_eq!(deserialized.outputs().len(), 1);
    }

    fn create_mock(config: crate::ProviderConfig) -> Result<Box<dyn Provider>> {
        if config.host == "broken.corp.com" {
            return Err(Error::Auth {
                provider: config.host,
                reason: "no token".into(),
                source: None,
            });
        }
        Ok(Box::new(MockProvider {
            kind: Kind::GitHub,
            host: config.host,
        }))
    }

    const COUNTED_HOST: &str = "counted.corp.com";

    static COUNTED_FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    static MOCK_REGISTRATION: Registration = Registration {
        kind: crate::HostKind::GitHub,
        presets: &[crate::Preset {
            host: "github.com",
            api_base: "https://api.github.com",
            token_env: "GITHUB_TOKEN",
        }],
        default_token_env: Some("GITHUB_TOKEN"),
        infer_api_base: |host| Some(format!("https://{host}/api/v3")),
        capabilities: crate::Capabilities {
            supports_graphql: true,
            supports_rest: true,
        },
        create: create_mock,
    };

    fn host_of_kind(kind: crate::HostKind) -> crate::HostConfig {
        crate::HostConfig {
            kind: Some(kind),
            api_base: Some("https://example.com/api".into()),
            token_env: Some("TOKEN".into()),
            api_mode: None,
            max_tags_per_repo: None,
        }
    }

    #[test]
    fn from_config_registers_the_preset_default_host() -> std::result::Result<(), Error> {
        let registry =
            Registry::from_config(&Config::default(), &[&MOCK_REGISTRATION], &Retention::Run)?;
        assert_eq!(registry.default_host(), Some("github.com"));
        let hosts: Vec<&str> = registry.providers().iter().map(Provider::host).collect();
        assert_eq!(hosts, ["github.com"]);
        Ok(())
    }

    #[test]
    fn from_config_registers_each_host_once() -> std::result::Result<(), Error> {
        let mut config = Config::default();
        for host in ["github.com", "ghe.corp.com"] {
            config
                .hosts
                .insert(host.into(), host_of_kind(crate::HostKind::GitHub));
        }
        let registry = Registry::from_config(&config, &[&MOCK_REGISTRATION], &Retention::Run)?;
        let mut hosts: Vec<&str> = registry.providers().iter().map(Provider::host).collect();
        hosts.sort_unstable();
        assert_eq!(hosts, ["ghe.corp.com", "github.com"]);
        Ok(())
    }

    #[test]
    fn from_config_rejects_a_kind_without_registration() {
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.corp.com".into(),
            host_of_kind(crate::HostKind::GitLab),
        );
        let result = Registry::from_config(&config, &[&MOCK_REGISTRATION], &Retention::Run);
        let Err(Error::InvalidConfig { path, .. }) = result else {
            panic!("expected InvalidConfig, got {result:?}");
        };
        assert_eq!(path, "host \"gitlab.corp.com\"");
    }

    #[test]
    fn from_config_returns_the_provider_creation_error() {
        let mut config = Config::default();
        config.hosts.insert(
            "broken.corp.com".into(),
            host_of_kind(crate::HostKind::GitHub),
        );
        let result = Registry::from_config(&config, &[&MOCK_REGISTRATION], &Retention::Run);
        assert!(
            matches!(result, Err(Error::Auth { ref provider, .. }) if provider == "broken.corp.com"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn from_config_caches_answers_for_the_given_retention() -> std::result::Result<(), Error>
    {
        let mut config = Config::default();
        config
            .hosts
            .insert(COUNTED_HOST.into(), host_of_kind(crate::HostKind::GitHub));
        let clock = crate::clock::ManualClock::new();
        let dyn_clock: std::sync::Arc<dyn crate::Clock> = clock.clone();
        let retention = Retention::Expiring {
            ttl: std::time::Duration::from_secs(60),
            clock: dyn_clock,
        };
        let registry = Registry::from_config(&config, &[&MOCK_REGISTRATION], &retention)?;
        let Some(provider) = registry.find(Some(COUNTED_HOST)) else {
            panic!("{COUNTED_HOST} is registered");
        };
        let repo = ActionRepoId::simple("actions", "checkout");
        let fetches = || COUNTED_FETCHES.load(std::sync::atomic::Ordering::SeqCst);

        provider.get_versions(&repo).await?;
        provider.get_versions(&repo).await?;
        assert_eq!(fetches(), 1);
        clock.advance(std::time::Duration::from_secs(61));
        provider.get_versions(&repo).await?;
        assert_eq!(fetches(), 2);
        Ok(())
    }
}
