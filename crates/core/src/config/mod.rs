//! Configuration types and loading
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml, etc.)
//! 4. User/global config (~/.config/actup/config.toml) - fills gaps only
//! 5. Defaults (lowest)

pub mod discovery;

use crate::Error;
use crate::policy;
use figment::Figment;
use figment::providers::{Env, Format, Json, Serialized, Toml, Yaml};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[allow(clippy::module_name_repetitions)]
pub use discovery::find_repo_config;
pub use discovery::{CONFIG_EXTENSIONS, iter_config_candidates, user_config_path};

/// # Actup configuration
/// Configuration options for the GitHub Actions-like updater.
///
/// Main configuration struct.
///
/// # Example Configuration
///
/// ```yaml
/// # .actup.toml - Conservative security-focused config
/// update_policy = "patch"
/// ref_style = "sha"
/// branch_policy = "warn"
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v4
/// # After:
/// # - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.1.1
/// ```
///
/// ```yaml
/// # .actup.toml - Permissive always-latest config
/// update_policy = "major"
/// ref_style = "tag"
/// allow_prerelease = true
///
/// # Result in workflow:
/// # Before:
/// # - uses: actions/checkout@v3
/// # After:
/// # - uses: actions/checkout@v4.2.0
/// ```
///
/// ```toml
/// # .actup.toml - Selective ignore patterns
/// ignore = [
///   "actions/checkout",              # Ignore all refs of this action
///   "my-org/*",                      # Ignore all actions from my-org
///   "dtolnay/rust-toolchain@1.91.0", # Ignore specific MSRV version
/// ]
///
/// # Result: dtolnay/rust-toolchain@stable is checked, @1.91.0 is ignored
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Global update policy
    pub update_policy: policy::Update,

    /// How to format refs when updating
    pub ref_style: policy::Style,

    /// How to handle branch references
    pub branch_policy: policy::Branch,

    /// Whether to consider prerelease versions
    pub allow_prerelease: bool,

    /// Actions to ignore (supports owner/repo, owner/repo@ref, and owner/* patterns)
    ///
    /// Examples:
    /// - `"actions/checkout"` - ignores all refs of this action
    /// - `"actions/checkout@v2"` - ignores only the v2 ref
    /// - `"my-org/*"` - ignores all actions from my-org
    /// - `"tool/action@1.91.0"` - ignores specific version (e.g., MSRV pin)
    pub ignore: Vec<String>,

    /// Per-action policy overrides (keyed by "owner/repo")
    #[serde(default, rename = "actions")]
    pub action_overrides: HashMap<String, ActionOverride>,

    /// Cache configuration
    pub cache: Cache,

    /// Provider configuration
    pub providers: Providers,

    /// Additional paths to scan (beyond defaults)
    ///
    /// By default, actup scans:
    /// - `.github/workflows/**/*.{yml,yaml}` (workflows)
    /// - `.github/actions/**/action.{yml,yaml}` (composite actions)
    ///
    /// Use `extra_paths` to scan additional locations:
    ///
    /// Examples:
    /// - `".gitea/workflows"` - Gitea workflow directory
    /// - `".woodpecker/*.yml"` - Woodpecker CI pipelines
    /// - `"ci/pipelines/**/*.yaml"` - Custom CI directory
    /// - `".drone.yml"` - Specific Drone CI config file
    /// - `"infra/terraform/github-actions.tf"` - GitHub Actions in Terraform
    pub extra_paths: Vec<String>,

    /// Path this config was loaded from (if any)
    #[serde(skip)]
    pub loaded_from: Option<PathBuf>,
}

/// # Per-action override configuration.
///
/// ## Example
/// ```toml
/// # .actup.toml - Per-action overrides
/// [actions."actions/checkout"]
/// update_policy = "major"
///
/// [actions."actions/cache"]
/// ignore = true
///
/// [actions."internal/action"]
/// ref_style = "sha"
/// ```
///
/// ## Result in workflow:
/// ```console
/// # actions/checkout@v3 -> actions/checkout@v4 (major allowed)
/// # actions/cache@v3 -> unchanged (ignored)
/// # internal/action@v1 -> internal/action@abc123... # v1.2.0 (pinned to SHA)
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ActionOverride {
    /// Override update policy for this action
    pub update_policy: Option<policy::Update>,

    /// Override ref style for this action
    pub ref_style: Option<policy::Style>,

    /// Whether to completely ignore this action
    pub ignore: bool,
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Cache {
    /// Cache time-to-live in seconds
    #[serde(
        serialize_with = "serialize_duration_secs",
        deserialize_with = "deserialize_duration_secs"
    )]
    #[schemars(with = "u64")]
    pub ttl: Duration,

    /// Custom cache directory (None = use OS default)
    pub path: Option<PathBuf>,

    /// Whether caching is enabled
    pub enabled: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(24 * 60 * 60), // 24 hours
            path: None,
            enabled: true,
        }
    }
}

/// Serialize Duration as seconds (u64)
fn serialize_duration_secs<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_u64(duration.as_secs())
}

/// Deserialize Duration from seconds (u64)
fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

/// API mode for fetching versions
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApiMode {
    /// Use GraphQL API for batched requests (requires auth token)
    #[default]
    Graphql,
    /// Use REST API with parallel requests
    Rest,
}

impl std::fmt::Display for ApiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graphql => write!(f, "graphql"),
            Self::Rest => write!(f, "rest"),
        }
    }
}

/// Provider kind identifiers.
///
/// Each variant corresponds to a provider crate that implements the [`Provider`](crate::Provider)
/// trait. Used for cache namespacing, registration lookup, and config validation.
///
/// Adding a new provider requires adding a variant here **and** a provider crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    /// GitHub / GitHub Enterprise
    Github,
    /// GitLab (gitlab.com or self-hosted)
    Gitlab,
    /// Gitea / Forgejo
    Gitea,
}

impl std::fmt::Display for HostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Github => write!(f, "github"),
            Self::Gitlab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
        }
    }
}

/// Global fetch settings applied to all providers unless overridden per-host.
///
/// Controls how actup communicates with provider APIs. Per-host overrides
/// in `HostConfig` take precedence over these values.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    /// API mode: `"graphql"` (batched, requires auth) or `"rest"` (parallel)
    pub api_mode: ApiMode,

    /// Maximum tags to fetch per repository (1–255).
    /// Stops early when a stable release satisfying policy is found.
    pub max_tags_per_repo: NonZeroU8,
}

impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: DEFAULT_MAX_TAGS_PER_REPO,
        }
    }
}

/// # Provider configuration
///
/// Default uses empty strings. The CLI layer sets actual defaults.
/// Core must remain provider-neutral.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Providers {
    /// Default provider kind
    pub default_kind: String,

    /// Default host (e.g., "github.com")
    pub default_host: String,

    /// Additional provider instances
    pub instances: Vec<ProviderInstance>,

    /// API mode: graphql (batched, requires token) or rest (parallel requests)
    pub api_mode: ApiMode,

    /// Maximum tags to fetch per repo (default: 10, minimum: 1)
    /// Fetches up to this many tags, or stops early when a stable release is found
    pub max_tags_per_repo: NonZeroU8,
}

/// Default maximum tags to fetch per repository
pub const DEFAULT_MAX_TAGS_PER_REPO: NonZeroU8 = match NonZeroU8::new(10) {
    Some(v) => v,
    None => panic!("DEFAULT_MAX_TAGS_PER_REPO must be non-zero"),
};

impl Default for Providers {
    fn default() -> Self {
        Self {
            default_kind: "github".into(),
            default_host: "github.com".into(),
            instances: Vec::new(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: DEFAULT_MAX_TAGS_PER_REPO,
        }
    }
}

/// # A configured provider instance
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderInstance {
    /// Provider kind (github, gitlab, gitea, etc.)
    pub kind: String,

    /// Host this instance serves
    pub host: String,

    /// API base URL (for enterprise/self-hosted)
    pub api_base: Option<String>,

    /// Environment variable name for token
    pub token_env: Option<String>,
}

impl Config {
    /// Create a new config with defaults
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load config with full layering support
    ///
    /// Precedence (highest to lowest):
    /// 1. Environment variables (ACTUP_*)
    /// 2. Explicit `config_path` (if provided)
    /// 3. Repo config (searched upward from `start_dir`)
    /// 4. User config (~/.config/actup/config.toml) - fills gaps only
    /// 5. Defaults
    ///
    /// # Errors
    /// Returns error if config file exists but is invalid.
    #[allow(clippy::option_if_let_else)] // Readability: mutating figment in if-let is clearer
    pub fn load(config_path: Option<&Path>, start_dir: &Path) -> Result<Self, Error> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));

        // User config fills gaps (lowest precedence file)
        if let Some(user_config) = user_config_path()
            && user_config.exists()
        {
            figment = figment.join(Toml::file(&user_config));
        }

        // Repo config (searched upward from `start_dir`)
        let loaded_from = if let Some(repo_config) = find_repo_config(start_dir) {
            figment = Self::merge_config_file(figment, &repo_config)?;
            Some(repo_config)
        } else {
            None
        };

        // Explicit --config path (high precedence)
        let loaded_from = if let Some(path) = config_path {
            figment = Self::merge_config_file(figment, path)?;
            Some(path.to_path_buf())
        } else {
            loaded_from
        };

        // Environment variables (highest precedence)
        figment = figment.merge(Env::prefixed("ACTUP_").split("_"));

        let mut config: Self = figment.extract().map_err(|e| Error::InvalidConfig {
            path: loaded_from
                .as_ref()
                .map_or_else(|| "config".into(), |p| p.display().to_string()),
            reason: e.to_string(),
        })?;

        config.loaded_from = loaded_from;
        Ok(config)
    }

    /// Merge a config file into figment based on extension
    fn merge_config_file(figment: Figment, path: &Path) -> Result<Figment, Error> {
        let ext = path.extension().and_then(|e| e.to_str());

        Ok(match ext {
            Some("yaml" | "yml") => figment.merge(Yaml::file(path)),
            Some("json") => figment.merge(Json::file(path)),
            Some("jsonc") => {
                // Read file, strip comments, parse as JSON
                let content = std::fs::read_to_string(path).map_err(|e| Error::InvalidConfig {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;

                let mut stripped = String::new();
                json_comments::StripComments::new(content.as_bytes())
                    .read_to_string(&mut stripped)
                    .map_err(|e| Error::InvalidConfig {
                        path: path.display().to_string(),
                        reason: format!("failed to strip comments: {e}"),
                    })?;

                figment.merge(Json::string(&stripped))
            }
            // Default to TOML for .toml or unknown extensions
            _ => figment.merge(Toml::file(path)),
        })
    }

    /// Discover and load config from standard locations (convenience method)
    ///
    /// Equivalent to `Config::load(None, start_dir)` but returns defaults on error.
    #[must_use]
    pub fn discover(start_dir: &Path) -> Self {
        Self::load(None, start_dir).unwrap_or_default()
    }

    /// Get the effective update policy for an action
    #[must_use]
    pub fn policy_for(&self, owner: &str, repo: &str) -> policy::Update {
        let key = format!("{owner}/{repo}");
        self.action_overrides
            .get(&key)
            .and_then(|o| o.update_policy)
            .unwrap_or(self.update_policy)
    }

    /// Get the effective ref style for an action
    #[must_use]
    pub fn ref_style_for(&self, owner: &str, repo: &str) -> policy::Style {
        let key = format!("{owner}/{repo}");
        self.action_overrides
            .get(&key)
            .and_then(|o| o.ref_style)
            .unwrap_or(self.ref_style)
    }

    /// Check if an action should be ignored.
    ///
    /// Supports multiple ignore patterns:
    /// - `owner/repo` - ignores all refs for this action
    /// - `owner/repo@ref` - ignores only this specific ref
    /// - `owner/*` - ignores all repos from this owner
    #[must_use]
    pub fn should_ignore(&self, owner: &str, repo: &str, ref_str: &str) -> bool {
        let full = format!("{owner}/{repo}");
        let full_with_ref = format!("{owner}/{repo}@{ref_str}");

        // Check explicit override
        if let Some(ovr) = self.action_overrides.get(&full)
            && ovr.ignore
        {
            return true;
        }

        // Check ignore list
        self.ignore.iter().any(|pattern| {
            // Match specific ref: owner/repo@ref
            if pattern.contains('@') && pattern == &full_with_ref {
                return true;
            }

            // Match repo (all refs): owner/repo
            if pattern == &full {
                return true;
            }

            // Match owner wildcard: owner/*
            if let Some(owner_pattern) = pattern.strip_suffix("/*") {
                return owner == owner_pattern;
            }

            false
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Config defaults tests

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.update_policy, policy::Update::Latest);
        assert_eq!(config.ref_style, policy::Style::Preserve);
        assert_eq!(config.branch_policy, policy::Branch::Ignore);
        assert!(!config.allow_prerelease);
        assert!(config.ignore.is_empty());
        assert!(config.action_overrides.is_empty());
        assert!(config.extra_paths.is_empty());
        assert!(config.loaded_from.is_none());
    }

    // should_ignore tests

    #[test]
    fn test_should_ignore() {
        let config = Config {
            ignore: vec!["actions/checkout".to_string(), "my-org/*".to_string()],
            ..Default::default()
        };

        assert!(config.should_ignore("actions", "checkout", "v1"));
        assert!(config.should_ignore("my-org", "anything", "v2"));
        assert!(!config.should_ignore("other", "repo", "v1"));
    }

    #[test]
    fn test_should_ignore_empty() {
        let config = Config::default();
        assert!(!config.should_ignore("any", "repo", "v1"));
    }

    #[test]
    fn test_should_ignore_wildcard_only() {
        let config = Config {
            ignore: vec!["org/*".to_string()],
            ..Default::default()
        };

        assert!(config.should_ignore("org", "repo1", "v1"));
        assert!(config.should_ignore("org", "repo2", "v2"));
        assert!(!config.should_ignore("other-org", "repo1", "v1"));
    }

    #[test]
    fn test_should_ignore_exact_match() {
        let config = Config {
            ignore: vec!["owner/exact-repo".to_string()],
            ..Default::default()
        };

        assert!(config.should_ignore("owner", "exact-repo", "v1"));
        assert!(!config.should_ignore("owner", "other-repo", "v1"));
    }

    #[test]
    fn test_should_ignore_specific_ref() {
        let config = Config {
            ignore: vec![
                "owner/repo@v1.91.0".to_string(),
                "another/action@main".to_string(),
            ],
            ..Default::default()
        };

        // Specific refs should be ignored
        assert!(config.should_ignore("owner", "repo", "v1.91.0"));
        assert!(config.should_ignore("another", "action", "main"));

        // Other refs for the same repo should NOT be ignored
        assert!(!config.should_ignore("owner", "repo", "v2"));
        assert!(!config.should_ignore("owner", "repo", "stable"));
        assert!(!config.should_ignore("another", "action", "v1"));
    }

    // policy_for tests

    #[test]
    fn test_policy_override() {
        let config = Config {
            update_policy: policy::Update::Minor,
            action_overrides: std::collections::HashMap::from([(
                "special/action".to_string(),
                ActionOverride {
                    update_policy: Some(policy::Update::Patch),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        assert_eq!(config.policy_for("normal", "action"), policy::Update::Minor);
        assert_eq!(
            config.policy_for("special", "action"),
            policy::Update::Patch
        );
    }

    #[test]
    fn test_policy_for_uses_default_when_no_override() {
        let config = Config {
            update_policy: policy::Update::Major,
            ..Default::default()
        };

        assert_eq!(config.policy_for("any", "action"), policy::Update::Major);
    }

    #[test]
    fn test_policy_for_override_none_uses_default() {
        let config = Config {
            update_policy: policy::Update::Minor,
            action_overrides: std::collections::HashMap::from([(
                "special/action".to_string(),
                ActionOverride {
                    update_policy: None, // No override specified
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        // Should use default since override is None
        assert_eq!(
            config.policy_for("special", "action"),
            policy::Update::Minor
        );
    }

    // Cache tests

    #[test]
    fn test_cache_config_default() {
        let cache = Cache::default();
        assert!(cache.enabled);
        assert_eq!(cache.ttl, Duration::from_secs(24 * 60 * 60));
        assert!(cache.path.is_none());
    }

    #[test]
    fn test_cache_config_custom_ttl() {
        let cache = Cache {
            ttl: Duration::from_secs(12 * 3600),
            ..Default::default()
        };
        assert_eq!(cache.ttl, Duration::from_secs(12 * 3600));
    }

    // ActionOverride tests

    #[test]
    fn test_action_override_default() {
        let override_cfg = ActionOverride::default();
        assert!(override_cfg.update_policy.is_none());
        assert!(override_cfg.ref_style.is_none());
        assert!(!override_cfg.ignore);
    }

    #[test]
    fn test_action_override_full() {
        let override_cfg = ActionOverride {
            update_policy: Some(policy::Update::Patch),
            ref_style: Some(policy::Style::Sha),
            ignore: true,
        };
        assert_eq!(override_cfg.update_policy, Some(policy::Update::Patch));
        assert_eq!(override_cfg.ref_style, Some(policy::Style::Sha));
        assert!(override_cfg.ignore);
    }

    // Providers tests

    #[test]
    fn test_providers_config_default() {
        let providers = Providers::default();
        assert_eq!(providers.default_kind, "github");
        assert_eq!(providers.default_host, "github.com");
        assert!(providers.instances.is_empty());
        assert_eq!(providers.api_mode, ApiMode::Graphql);
        assert_eq!(providers.max_tags_per_repo, DEFAULT_MAX_TAGS_PER_REPO);
    }

    // HostKind tests

    #[test]
    fn test_hostkind_serde_roundtrip() {
        #[allow(clippy::unwrap_used)]
        for (variant, expected_str) in [
            (HostKind::Github, "\"github\""),
            (HostKind::Gitlab, "\"gitlab\""),
            (HostKind::Gitea, "\"gitea\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");

            let back: HostKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn test_hostkind_display() {
        assert_eq!(HostKind::Github.to_string(), "github");
        assert_eq!(HostKind::Gitlab.to_string(), "gitlab");
        assert_eq!(HostKind::Gitea.to_string(), "gitea");
    }

    #[test]
    fn test_hostkind_rejects_unknown() {
        let result = serde_json::from_str::<HostKind>("\"bitbucket\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_hostkind_case_sensitive() {
        // serde rename_all = "lowercase" means uppercase should fail
        let result = serde_json::from_str::<HostKind>("\"Github\"");
        assert!(result.is_err());

        let result = serde_json::from_str::<HostKind>("\"GITHUB\"");
        assert!(result.is_err());
    }

    #[test]
    fn test_hostkind_hashable() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HostKind::Github);
        set.insert(HostKind::Gitlab);
        set.insert(HostKind::Gitea);
        set.insert(HostKind::Github); // duplicate
        assert_eq!(set.len(), 3);
    }

    // FetchSettings tests

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_default() {
        let fs = FetchSettings::default();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_roundtrip_defaults() {
        let fs = FetchSettings::default();
        let json = serde_json::to_string(&fs).unwrap();
        let back: FetchSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_mode, ApiMode::Graphql);
        assert_eq!(back.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_custom_values() {
        let json = r#"{"api_mode":"rest","max_tags_per_repo":50}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 50);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_partial_uses_defaults() {
        // Only api_mode specified — max_tags_per_repo should default
        let json = r#"{"api_mode":"rest"}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 10);

        // Only max_tags_per_repo — api_mode should default
        let json = r#"{"max_tags_per_repo":25}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 25);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_serde_empty_uses_defaults() {
        let json = "{}";
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[test]
    fn test_fetch_settings_rejects_zero_max_tags() {
        let json = r#"{"max_tags_per_repo":0}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_fetch_settings_deny_unknown_fields() {
        let json = r#"{"api_mode":"graphql","unknown_field":true}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_max_tags_boundary() {
        // NonZeroU8 max = 255
        let json = r#"{"max_tags_per_repo":255}"#;
        let fs: FetchSettings = serde_json::from_str(json).unwrap();
        assert_eq!(fs.max_tags_per_repo.get(), 255);

        // 256 exceeds u8 range
        let json = r#"{"max_tags_per_repo":256}"#;
        let result = serde_json::from_str::<FetchSettings>(json);
        assert!(result.is_err());
    }
}
