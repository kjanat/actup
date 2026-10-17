//! Configuration types and loading
//!
//! Configuration is loaded from multiple sources with precedence:
//! 1. Environment variables (ACTUP_*) - highest
//! 2. Explicit --config path
//! 3. Repo configs (actup.toml, .actup.toml, .config/actup.toml, etc.)
//! 4. User/global config (~/.config/actup/config.toml) - fills gaps only
//! 5. Defaults (lowest)

pub mod discovery;
pub mod registration;
pub mod resolution;
pub mod validation;

use std::{
    collections::HashMap,
    io::Read,
    num::NonZeroU8,
    path::{Path, PathBuf},
    time::Duration,
};

#[allow(clippy::module_name_repetitions)]
pub use discovery::find_repo_config;
pub use discovery::{CONFIG_EXTENSIONS, iter_config_candidates, user_config_path};
use figment::{
    Figment,
    providers::{Env, Format, Json, Serialized, Toml, Yaml},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Error, policy};

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
    /// How far to bump outdated version refs.
    ///
    /// `patch` stays on `major.minor`, `minor` stays on `major`, `major`
    /// allows a new major, `latest` takes the newest stable overall.
    ///
    /// ```toml
    /// update_policy = "minor"   # v1.2.3 -> v1.5.0, never v2
    /// ```
    pub update_policy: policy::Update,

    /// How a ref is written when updating.
    ///
    /// `preserve` keeps the current style (tag stays a tag, SHA stays a SHA),
    /// `tag` rewrites to the version tag, `sha` pins to the commit SHA with a
    /// `# vX.Y.Z` comment.
    ///
    /// ```toml
    /// ref_style = "sha"   # actions/checkout@v4 -> @<sha> # v4.2.0
    /// ```
    pub ref_style: policy::Style,

    /// How to handle floating branch refs like `@main` or `@v2`.
    ///
    /// `ignore` leaves them untouched (default), `warn` flags them as
    /// floating, `update` pins the branch to its current HEAD SHA.
    ///
    /// ```toml
    /// branch_policy = "warn"   # @main -> flagged, pin to a tag or SHA
    /// ```
    pub branch_policy: policy::Branch,

    /// Consider prerelease tags (e.g. `v2.0.0-rc.1`) when picking a target.
    ///
    /// ```toml
    /// allow_prerelease = false   # skip -rc/-beta tags unless already on one
    /// ```
    pub allow_prerelease: bool,

    /// Actions to ignore (supports owner/repo, owner/repo@ref, and owner/* patterns)
    ///
    /// Examples:
    /// - `"actions/checkout"` - ignores all refs of this action
    /// - `"actions/checkout@v2"` - ignores only the v2 ref
    /// - `"my-org/*"` - ignores all actions from my-org
    /// - `"tool/action@1.91.0"` - ignores specific version (e.g., MSRV pin)
    pub ignore: Vec<String>,

    /// Per-action policy overrides, keyed by `owner/repo`.
    ///
    /// Each entry can override `update_policy`, `ref_style`, `branch_policy`,
    /// or `ignore` for just that action.
    ///
    /// ```toml
    /// [actions."actions/checkout"]
    /// update_policy = "patch"    # only patch-bump this one
    /// ```
    #[serde(default, rename = "actions")]
    pub action_overrides: HashMap<String, ActionOverride>,

    /// Version-cache settings (TTL, location, on/off).
    ///
    /// ```toml
    /// [cache]
    /// ttl = 86400        # re-check a repo at most once a day
    /// enabled = true
    /// ```
    pub cache: Cache,

    /// Host used for bare `owner/repo` refs that carry no host prefix.
    ///
    /// Absent resolves to `github.com` via the built-in preset.
    ///
    /// ```toml
    /// default_host = "github.com"
    /// ```
    pub default_host: Option<String>,

    /// API fallback settings, used only when the git protocol can't list a repo.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "graphql"       # or "rest"
    /// max_tags_per_repo = 10
    /// ```
    pub fetch: FetchSettings,

    /// Per-host provider config, keyed by lowercase hostname.
    ///
    /// Used for GitHub Enterprise, self-hosted GitLab/Gitea, and custom tokens.
    ///
    /// ```toml
    /// [hosts."ghe.corp.internal"]
    /// kind = "github"
    /// api_base = "https://ghe.corp.internal/api/v3"
    /// token_env = "GHE_TOKEN"
    /// ```
    pub hosts: HashMap<String, HostConfig>,

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

    /// Override branch-ref handling for this action.
    ///
    /// Same values as the global `branch_policy`: `ignore` leaves branch refs
    /// untouched, `warn` flags them as floating, `update` pins them to the
    /// branch's current HEAD SHA.
    ///
    /// ```toml
    /// branch_policy = "ignore"           # quiet everywhere by default
    ///
    /// [actions."kjanat/runner"]
    /// branch_policy = "update"           # but pin this one to its master SHA
    /// ```
    pub branch_policy: Option<policy::Branch>,

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
            ttl: Duration::from_hours(24),
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
    /// One batched GraphQL query for all repos. Fast, but needs an auth token.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "graphql"
    /// ```
    #[default]
    Graphql,
    /// Parallel REST requests, one per repo. Works without a token.
    ///
    /// ```toml
    /// [fetch]
    /// api_mode = "rest"
    /// ```
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
    GitHub,
    /// GitLab (gitlab.com or self-hosted)
    GitLab,
    /// Gitea / Forgejo
    Gitea,
}

impl std::fmt::Display for HostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitHub => write!(f, "github"),
            Self::GitLab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
        }
    }
}

/// Global fetch settings applied to all providers unless overridden per-host.
///
/// Versions come from the git wire protocol, which needs no API and lists every
/// tag. These settings govern the HTTP API fallback used when a host refuses a
/// git connection. Per-host overrides in `HostConfig` take precedence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    /// API fallback mode: `"graphql"` (batched, requires auth) or `"rest"` (parallel)
    pub api_mode: ApiMode,

    /// Maximum tags the API fallback fetches per repository (1–255).
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

/// Per-host configuration for a single provider host.
///
/// All fields are optional: they act as overrides on top of provider preset
/// defaults and global `[fetch]` settings. After resolution, `kind`, `api_base`,
/// and `token_env` must all be present (validated post-merge).
///
/// Used as values in the `[hosts."hostname"]` config map.
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Provider kind. Required for non-preset hosts; implied by preset for known hosts.
    pub kind: Option<HostKind>,

    /// API base URL. Required after merge (may be inferred by provider registration).
    /// Must include scheme (`http://` or `https://`), no query or fragment.
    pub api_base: Option<String>,

    /// Env var name for auth token. Required after merge (may be defaulted by preset).
    pub token_env: Option<String>,

    /// Override global `[fetch].api_mode` for this host.
    pub api_mode: Option<ApiMode>,

    /// Override global `[fetch].max_tags_per_repo` for this host.
    pub max_tags_per_repo: Option<NonZeroU8>,
}

/// Default maximum tags to fetch per repository
pub const DEFAULT_MAX_TAGS_PER_REPO: NonZeroU8 = match NonZeroU8::new(10) {
    Some(v) => v,
    None => panic!("DEFAULT_MAX_TAGS_PER_REPO must be non-zero"),
};

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

    /// Get the effective branch policy for an action
    #[must_use]
    pub fn branch_policy_for(&self, owner: &str, repo: &str) -> policy::Branch {
        let key = format!("{owner}/{repo}");
        self.action_overrides
            .get(&key)
            .and_then(|o| o.branch_policy)
            .unwrap_or(self.branch_policy)
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
        assert_eq!(config.ignore, Vec::<String>::new());
        assert!(config.action_overrides.is_empty());
        assert_eq!(config.extra_paths, Vec::<String>::new());
        assert!(config.loaded_from.is_none());
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
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
        assert_eq!(cache.ttl, Duration::from_hours(24));
        assert!(cache.path.is_none());
    }

    #[test]
    fn test_cache_config_custom_ttl() {
        let cache = Cache {
            ttl: Duration::from_hours(12),
            ..Default::default()
        };
        assert_eq!(cache.ttl, Duration::from_hours(12));
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
            branch_policy: Some(policy::Branch::Update),
            ignore: true,
        };
        assert_eq!(override_cfg.update_policy, Some(policy::Update::Patch));
        assert_eq!(override_cfg.ref_style, Some(policy::Style::Sha));
        assert_eq!(override_cfg.branch_policy, Some(policy::Branch::Update));
        assert!(override_cfg.ignore);
    }

    // HostKind tests

    #[test]
    fn test_hostkind_serde_roundtrip() {
        #[allow(clippy::unwrap_used)]
        for (variant, expected_str) in [
            (HostKind::GitHub, "\"github\""),
            (HostKind::GitLab, "\"gitlab\""),
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
        assert_eq!(HostKind::GitHub.to_string(), "github");
        assert_eq!(HostKind::GitLab.to_string(), "gitlab");
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
        set.insert(HostKind::GitHub);
        set.insert(HostKind::GitLab);
        set.insert(HostKind::Gitea);
        set.insert(HostKind::GitHub); // duplicate
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

    // HostConfig tests

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_all_fields() {
        let json = r#"{
            "kind": "github",
            "api_base": "https://ghe.corp.com/api/v3",
            "token_env": "GHE_TOKEN",
            "api_mode": "rest",
            "max_tags_per_repo": 50
        }"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_no_fields() {
        let json = "{}";
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_partial_kind_only() {
        let json = r#"{"kind": "gitlab"}"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitLab));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
        assert_eq!(hc.api_mode, None);
        assert_eq!(hc.max_tags_per_repo, None);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_partial_fetch_overrides_only() {
        let json = r#"{"api_mode": "rest", "max_tags_per_repo": 25}"#;
        let hc: HostConfig = serde_json::from_str(json).unwrap();
        assert_eq!(hc.kind, None);
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(25));
    }

    #[test]
    fn test_hostconfig_deny_unknown_fields() {
        let json = r#"{"kind": "github", "bogus": true}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_invalid_kind() {
        let json = r#"{"kind": "bitbucket"}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hostconfig_rejects_zero_max_tags() {
        let json = r#"{"max_tags_per_repo": 0}"#;
        let result = serde_json::from_str::<HostConfig>(json);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_serde_roundtrip() {
        let hc = HostConfig {
            kind: Some(HostKind::Gitea),
            api_base: Some("https://gitea.local/api/v1".into()),
            token_env: Some("GITEA_TOKEN".into()),
            api_mode: Some(ApiMode::Rest),
            max_tags_per_repo: NonZeroU8::new(100),
        };
        let json = serde_json::to_string(&hc).unwrap();
        let back: HostConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, hc.kind);
        assert_eq!(back.api_base, hc.api_base);
        assert_eq!(back.token_env, hc.token_env);
        assert_eq!(back.api_mode, hc.api_mode);
        assert_eq!(back.max_tags_per_repo, hc.max_tags_per_repo);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_each_kind_variant() {
        for (variant, name) in [
            (HostKind::GitHub, "github"),
            (HostKind::GitLab, "gitlab"),
            (HostKind::Gitea, "gitea"),
        ] {
            let json = format!(r#"{{"kind": "{name}"}}"#);
            let hc: HostConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(hc.kind, Some(variant), "kind={name}");
        }
    }

    // TOML deserialization tests (realistic config fragments)

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostkind_toml_deserialization() {
        #[derive(Deserialize)]
        struct Wrapper {
            kind: HostKind,
        }
        for (variant, name) in [
            (HostKind::GitHub, "github"),
            (HostKind::GitLab, "gitlab"),
            (HostKind::Gitea, "gitea"),
        ] {
            let toml_str = format!("kind = \"{name}\"");
            let w: Wrapper = toml::from_str(&toml_str).unwrap();
            assert_eq!(w.kind, variant, "TOML kind={name}");
        }
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_toml_deserialization() {
        let toml_str = r#"
            api_mode = "rest"
            max_tags_per_repo = 42
        "#;
        let fs: FetchSettings = toml::from_str(toml_str).unwrap();
        assert_eq!(fs.api_mode, ApiMode::Rest);
        assert_eq!(fs.max_tags_per_repo.get(), 42);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_fetch_settings_toml_empty_uses_defaults() {
        let fs: FetchSettings = toml::from_str("").unwrap();
        assert_eq!(fs.api_mode, ApiMode::Graphql);
        assert_eq!(fs.max_tags_per_repo.get(), 10);
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_full() {
        let toml_str = r#"
            kind              = "github"
            api_base          = "https://ghe.corp.com/api/v3"
            token_env         = "GHE_TOKEN"
            api_mode          = "rest"
            max_tags_per_repo = 50
        "#;
        let hc: HostConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(hc.kind, Some(HostKind::GitHub));
        assert_eq!(hc.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(hc.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(hc.api_mode, Some(ApiMode::Rest));
        assert_eq!(hc.max_tags_per_repo.map(NonZeroU8::get), Some(50));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_minimal() {
        let toml_str = r#"kind = "gitea""#;
        let hc: HostConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(hc.kind, Some(HostKind::Gitea));
        assert_eq!(hc.api_base, None);
        assert_eq!(hc.token_env, None);
    }

    #[test]
    fn test_hostconfig_toml_deny_unknown_fields() {
        let toml_str = r#"
            kind = "github"
            typo_field = "oops"
        "#;
        let result = toml::from_str::<HostConfig>(toml_str);
        assert!(result.is_err());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_hostconfig_toml_in_hosts_map() {
        // Test as it would appear in actual config: [hosts."hostname"]
        #[derive(Deserialize)]
        struct Outer {
            hosts: HashMap<String, HostConfig>,
        }
        let toml_str = r#"
            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"

            [hosts."gitlab.internal"]
            kind      = "gitlab"
            api_base  = "https://gitlab.internal/api/v4"
            token_env = "GITLAB_TOKEN"
            api_mode  = "rest"
        "#;
        let outer: Outer = toml::from_str(toml_str).unwrap();
        assert_eq!(outer.hosts.len(), 2);

        let ghe = &outer.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
        assert_eq!(ghe.token_env.as_deref(), Some("GHE_TOKEN"));
        assert_eq!(ghe.api_mode, None);

        let gl = &outer.hosts["gitlab.internal"];
        assert_eq!(gl.kind, Some(HostKind::GitLab));
        assert_eq!(gl.api_mode, Some(ApiMode::Rest));
    }

    // Config-level TOML tests (new fields)

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_new_fields() {
        let toml_str = r#"
            default_host = "github.com"

            [fetch]
            api_mode          = "rest"
            max_tags_per_repo = 42

            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
        assert_eq!(config.fetch.api_mode, ApiMode::Rest);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 42);
        assert_eq!(config.hosts.len(), 1);
        let ghe = &config.hosts["ghe.corp.com"];
        assert_eq!(ghe.kind, Some(HostKind::GitHub));
        assert_eq!(ghe.api_base.as_deref(), Some("https://ghe.corp.com/api/v3"));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_empty_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_only_fetch() {
        let toml_str = r#"
            [fetch]
            api_mode = "graphql"
            max_tags_per_repo = 100
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.default_host.is_none());
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 100);
        assert!(config.hosts.is_empty());
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_config_toml_multiple_hosts() {
        let toml_str = r#"
            default_host = "ghe.corp.com"

            [hosts."ghe.corp.com"]
            kind      = "github"
            api_base  = "https://ghe.corp.com/api/v3"
            token_env = "GHE_TOKEN"
            api_mode  = "rest"

            [hosts."gitlab.internal"]
            kind      = "gitlab"
            api_base  = "https://gitlab.internal/api/v4"
            token_env = "GITLAB_TOKEN"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.com"));
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts["ghe.corp.com"].api_mode, Some(ApiMode::Rest));
        assert_eq!(config.hosts["gitlab.internal"].kind, Some(HostKind::GitLab));
    }
}
